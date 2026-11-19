//! Query execution.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use corax_core::{Shared, World};

use crate::filter::Filter;
use crate::query::{status, AggOp, Query, Request, StatsColumn};
use crate::render::{self, OutputFormat};
use crate::tables::{self, Auth, Column, Ctx, Preloaded, Row};
use crate::value::Value;

/// The answer to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// Prefix the body with the 16 byte `fixed16` header.
    pub fixed16: bool,
    /// Keep the connection open for further requests.
    pub keepalive: bool,
    /// `false` for `COMMAND` requests, which produce no output at all.
    pub has_output: bool,
}

impl Response {
    fn error(status: u16, message: &str, fixed16: bool, keepalive: bool) -> Self {
        Response { status, body: format!("{message}\n").into_bytes(), fixed16, keepalive, has_output: true }
    }

    /// The bytes to send to the client.
    pub fn to_bytes(&self) -> Vec<u8> {
        if !self.has_output {
            return Vec::new();
        }
        if !self.fixed16 {
            return self.body.clone();
        }
        let mut out = format!("{:03} {:>11}\n", self.status, self.body.len()).into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Execute a request synchronously (no `WaitTimeout` support; waits are skipped).
pub fn execute(shared: &Shared, request: &str) -> Response {
    match Request::parse(request) {
        Request::Get(q) => run_query(shared, &q),
        Request::Command(cmd) => command(shared, &cmd),
        Request::Invalid { message, fixed16, keepalive } => {
            Response::error(status::BAD_REQUEST, &message, fixed16, keepalive)
        }
    }
}

/// Execute a request, honouring `WaitObject`/`WaitCondition`/`WaitTimeout`.
pub async fn execute_async(shared: &Shared, request: &str) -> Response {
    match Request::parse(request) {
        Request::Get(q) => {
            if q.error.is_none() && q.wait_condition.is_some() {
                if let Err(e) = wait_for(shared, &q).await {
                    return Response::error(status::BAD_REQUEST, &e, q.fixed16, q.keepalive);
                }
            }
            run_query(shared, &q)
        }
        Request::Command(cmd) => command(shared, &cmd),
        Request::Invalid { message, fixed16, keepalive } => {
            Response::error(status::BAD_REQUEST, &message, fixed16, keepalive)
        }
    }
}

fn command(shared: &Shared, cmd: &str) -> Response {
    crate::commands::handle(shared, cmd);
    Response { status: status::OK, body: Vec::new(), fixed16: false, keepalive: true, has_output: false }
}

fn resolve_auth(world: &World, user: Option<&str>) -> Auth {
    match user {
        None => Auth::All,
        Some(name) => world.contact_by_name.get(name).map(|c| Auth::Contact(*c)).unwrap_or(Auth::Nobody),
    }
}

/// Evaluate the wait condition against the wait object.
fn wait_satisfied(shared: &Shared, q: &Query, cond: &Filter) -> Result<bool, String> {
    let paths = shared.paths();
    let world = shared.world.read();
    let ctx = Ctx { world: &world, paths: &paths, now: corax_core::now(), auth: Auth::All };
    let obj = q.wait_object.as_deref().map(str::trim);
    let row = match (q.table.as_str(), obj) {
        ("hosts", Some(name)) => world.find_host(name).map(|h| Row::Host(world.host(h))),
        ("services", Some(key)) => {
            let (h, s) = key.split_once(';').or_else(|| key.split_once(' ')).unwrap_or((key, ""));
            world.find_service(h, s).map(|s| Row::Service(world.service(s)))
        }
        ("status", _) => Some(Row::Status),
        (_, None) => Some(Row::Status),
        (_, Some(_)) => None,
    };
    match row {
        Some(r) => Ok(cond.accepts(&ctx, &r)),
        None => Err(format!("primary key '{}' not found or not supported by this table", obj.unwrap_or(""))),
    }
}

/// Block until the query's wait condition holds (or the timeout expires).
pub async fn wait_for(shared: &Shared, q: &Query) -> Result<(), String> {
    let Some(cond) = &q.wait_condition else { return Ok(()) };
    // WaitTimeout 0 means "forever" in Livestatus; cap it to keep clients sane.
    let timeout =
        if q.wait_timeout_ms == 0 { Duration::from_secs(3600) } else { Duration::from_millis(q.wait_timeout_ms) };
    let deadline = Instant::now() + timeout;
    let mut changes = shared.subscribe_changes();
    loop {
        if wait_satisfied(shared, q, cond)? {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        match tokio::time::timeout(remaining, changes.changed()).await {
            Ok(Ok(())) => continue,
            // sender gone or timeout: answer with the current state
            _ => return Ok(()),
        }
    }
}

/// State of one stats column.
enum Agg {
    Count(i64),
    Num { op: AggOp, sum: f64, sumsq: f64, count: u32, value: f64, first: bool },
    Perf(BTreeMap<String, Box<Agg>>, AggOp),
}

impl Agg {
    fn new(s: &StatsColumn) -> Agg {
        match s {
            StatsColumn::Count(_) => Agg::Count(0),
            StatsColumn::Agg(op, col) if col.ty == crate::value::ColumnType::String => Agg::Perf(BTreeMap::new(), *op),
            StatsColumn::Agg(op, _) => Agg::num(*op),
        }
    }

    fn num(op: AggOp) -> Agg {
        Agg::Num { op, sum: 0.0, sumsq: 0.0, count: 0, value: 0.0, first: true }
    }

    fn update_num(&mut self, x: f64) {
        if let Agg::Num { op, sum, sumsq, count, value, first } = self {
            *count += 1;
            match op {
                AggOp::Sum | AggOp::Avg => *sum += x,
                AggOp::Std => {
                    *sum += x;
                    *sumsq += x * x;
                }
                AggOp::SumInv | AggOp::AvgInv => *sum += 1.0 / x,
                AggOp::Min => {
                    if *first || x < *value {
                        *value = x;
                    }
                }
                AggOp::Max => {
                    if *first || x > *value {
                        *value = x;
                    }
                }
            }
            *first = false;
        }
    }

    fn value_f64(&self) -> f64 {
        match self {
            Agg::Num { op, sum, sumsq, count, value, .. } => {
                let n = f64::from(*count);
                match op {
                    AggOp::Sum | AggOp::SumInv => *sum,
                    AggOp::Avg | AggOp::AvgInv => *sum / n,
                    AggOp::Std => {
                        let mean = *sum / n;
                        ((*sumsq / n) - mean * mean).sqrt()
                    }
                    AggOp::Min | AggOp::Max => *value,
                }
            }
            Agg::Count(c) => *c as f64,
            Agg::Perf(..) => 0.0,
        }
    }

    fn output(&self) -> Value {
        match self {
            Agg::Count(c) => Value::Int(*c),
            Agg::Num { .. } => Value::Float(self.value_f64()),
            Agg::Perf(vars, _) => Value::Str(
                vars.iter()
                    .map(|(k, a)| (k, a.value_f64()))
                    .filter(|(_, v)| v.is_finite())
                    .map(|(k, v)| format!("{k}={v:.6}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        }
    }
}

fn update_stats(ctx: &Ctx<'_>, row: &Row<'_>, stats: &[StatsColumn], aggs: &mut [Agg]) {
    for (s, a) in stats.iter().zip(aggs.iter_mut()) {
        match (s, a) {
            (StatsColumn::Count(f), Agg::Count(c)) => {
                if f.accepts(ctx, row) {
                    *c += 1;
                }
            }
            (StatsColumn::Agg(_, col), Agg::Perf(vars, op)) => {
                if let Value::Str(perf) = (col.get)(ctx, row) {
                    for (name, x) in perf_pairs(&perf) {
                        vars.entry(name).or_insert_with(|| Box::new(Agg::num(*op))).update_num(x);
                    }
                }
            }
            (StatsColumn::Agg(_, col), a) => a.update_num((col.get)(ctx, row).as_f64()),
            _ => {}
        }
    }
}

fn perf_pairs(perf: &str) -> Vec<(String, f64)> {
    perf.split_whitespace()
        .filter_map(|tok| {
            let (k, v) = tok.split_once('=')?;
            let num: String =
                v.chars().take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')).collect();
            Some((k.to_string(), num.parse().ok()?))
        })
        .collect()
}

/// Sort key for group-by rows (Livestatus orders groups by their rendered values).
fn group_key(values: &[Value]) -> Vec<u8> {
    render::render(OutputFormat::BrokenCsv, Default::default(), std::slice::from_ref(&values.to_vec()))
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(_) | Value::Int(_), Value::Float(_) | Value::Int(_)) => {
            a.as_f64().partial_cmp(&b.as_f64()).unwrap_or(std::cmp::Ordering::Equal)
        }
        _ => a.as_text().cmp(&b.as_text()),
    }
}

/// Execute a parsed `GET` query.
pub fn run_query(shared: &Shared, q: &Query) -> Response {
    crate::stats().count_query();
    if let Some((code, msg)) = &q.error {
        return Response::error(*code, msg, q.fixed16, q.keepalive);
    }
    let paths = shared.paths();
    let period = if q.table == "log" { q.filter.int_bounds("time") } else { (i64::MIN, i64::MAX) };
    let pre: Preloaded = tables::preload(&q.table, &paths, period);

    let order_cols: Vec<&Column> = q.order_by.iter().map(|(c, _)| c).collect();
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut sort_keys: Vec<Vec<Value>> = Vec::new();
    let mut groups: BTreeMap<Vec<u8>, (Vec<Value>, Vec<Agg>)> = BTreeMap::new();
    let mut single: Vec<Agg> = q.stats.iter().map(Agg::new).collect();
    {
        let world = shared.world.read();
        let labels = tables::collect_labels(&q.table, &world);
        let ctx = Ctx {
            world: &world,
            paths: &paths,
            now: corax_core::now(),
            auth: resolve_auth(&world, q.auth_user.as_deref()),
        };
        let limit = if q.order_by.is_empty() { q.limit } else { None };
        let mut matched = 0usize;
        tables::for_each_row(&q.table, &ctx, &pre, &labels, &mut |row| {
            if let Some(l) = limit {
                if matched >= l {
                    return false;
                }
            }
            if !q.filter.accepts(&ctx, &row) {
                return true;
            }
            matched += 1;
            if q.stats.is_empty() {
                rows.push(q.columns.iter().map(|c| (c.get)(&ctx, &row)).collect());
                if !order_cols.is_empty() {
                    sort_keys.push(order_cols.iter().map(|c| (c.get)(&ctx, &row)).collect());
                }
            } else if q.columns.is_empty() {
                update_stats(&ctx, &row, &q.stats, &mut single);
            } else {
                let values: Vec<Value> = q.columns.iter().map(|c| (c.get)(&ctx, &row)).collect();
                let key = group_key(&values);
                let entry = groups.entry(key).or_insert_with(|| (values, q.stats.iter().map(Agg::new).collect()));
                update_stats(&ctx, &row, &q.stats, &mut entry.1);
            }
            true
        });
    }

    if !q.order_by.is_empty() && q.stats.is_empty() {
        let mut idx: Vec<usize> = (0..rows.len()).collect();
        idx.sort_by(|&a, &b| {
            for (i, (_, desc)) in q.order_by.iter().enumerate() {
                let o = compare_values(&sort_keys[a][i], &sort_keys[b][i]);
                let o = if *desc { o.reverse() } else { o };
                if o.is_ne() {
                    return o;
                }
            }
            std::cmp::Ordering::Equal
        });
        let mut sorted: Vec<Vec<Value>> = idx.into_iter().map(|i| std::mem::take(&mut rows[i])).collect();
        if let Some(l) = q.limit {
            sorted.truncate(l);
        }
        rows = sorted;
    }

    let mut out: Vec<Vec<Value>> = Vec::with_capacity(rows.len() + 1);
    if q.column_headers {
        let mut header: Vec<Value> = q.columns.iter().map(|c| Value::str(&c.name)).collect();
        header.extend((1..=q.stats.len()).map(|i| Value::Str(format!("stats_{i}"))));
        out.push(header);
    }
    if q.stats.is_empty() {
        out.extend(rows);
    } else if q.columns.is_empty() {
        out.push(single.iter().map(Agg::output).collect());
    } else {
        for (_, (mut values, aggs)) in groups {
            values.extend(aggs.iter().map(Agg::output));
            out.push(values);
        }
    }
    // rrddata needs file and socket I/O: done here, without holding the world lock.
    let data_rows = usize::from(q.column_headers);
    crate::rrd::resolve(&paths, &q.columns, &mut out[data_rows..]);
    let body = render::render(q.output, q.separators, &out);
    Response { status: status::OK, body, fixed16: q.fixed16, keepalive: q.keepalive, has_output: true }
}
