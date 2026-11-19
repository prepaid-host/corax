//! Historic metric data: the dynamic `rrddata` columns.
//!
//! The Checkmk GUI draws every graph from columns like
//!
//! ```text
//! rrddata:util:util.max,1.000000,*:1795126680:1795130280:60
//! ```
//!
//! i.e. `rrddata:TITLE:RPN:START:END:RESOLUTION[:MAX_ENTRIES]`. The answer is a list
//! `[start, end, step, value, value, ...]`. Predictive levels of Checkmk checks read
//! their history the same way.
//!
//! As in the original Livestatus, the data comes from the PNP4Nagios RRDs
//! (`<pnp_path>/<host>/<service>_<metric>.rrd`, one data source named `1`) and is
//! read through the site's rrdcached with the `FETCHBIN` command, so that values
//! still cached in rrdcached are included. The RPN expression is applied to every
//! data point and the result is consolidated to the requested resolution.
//!
//! The data is fetched after the world lock has been released: while the lock is
//! held, an `rrddata` cell only records which object it belongs to (see
//! [`object_key`]) and [`resolve`] replaces it with the data afterwards.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use corax_core::Paths;

use crate::value::Value;

/// Service description used for host metrics.
pub const HOST_SERVICE: &str = "_HOST_";

/// Timeout for reading from and writing to rrdcached.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Parsed arguments of an `rrddata` column (everything after the title).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrdArgs {
    pub rpn: String,
    pub start: i64,
    pub end: i64,
    pub resolution: u64,
    /// Accepted for compatibility; like the original Livestatus, it does not limit
    /// the number of returned values.
    pub max_entries: u64,
}

impl RrdArgs {
    /// Parse `RPN:START:END:RESOLUTION[:MAX_ENTRIES]`.
    pub fn parse(args: &str) -> Result<Self, String> {
        let mut fields = args.split(':');
        let mut next_non_empty = |what: &str| -> Result<&str, String> {
            match fields.next() {
                Some(f) if !f.is_empty() => Ok(f),
                _ => Err(format!("missing {what}")),
            }
        };
        let rpn = next_non_empty("RPN expression")?.to_string();
        let start = parse_time(next_non_empty("start time")?, "start time")?;
        let end = parse_time(next_non_empty("end time")?, "end time")?;
        let resolution = parse_uint(next_non_empty("resolution")?, "resolution")?;
        let max_entries = match fields.next() {
            None | Some("") => 400,
            Some(f) => parse_uint(f, "maximum entries")?,
        };
        if fields.next().is_some() {
            return Err("too many arguments".into());
        }
        Ok(RrdArgs { rpn, start, end, resolution, max_entries })
    }
}

fn parse_time(s: &str, what: &str) -> Result<i64, String> {
    // Integral seconds are the norm; fractional seconds are truncated.
    s.parse::<i64>()
        .ok()
        .or_else(|| s.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| f as i64))
        .ok_or_else(|| format!("invalid number for {what}"))
}

fn parse_uint(s: &str, what: &str) -> Result<u64, String> {
    s.parse::<u64>().map_err(|_| format!("invalid number for {what}"))
}

/// Consolidation function, selected by a `.max`, `.min` or `.average` suffix of the
/// metric name (default: `MAX`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cf {
    Max,
    Min,
    Average,
}

impl Cf {
    pub fn rrd_name(self) -> &'static str {
        match self {
            Cf::Max => "MAX",
            Cf::Min => "MIN",
            Cf::Average => "AVERAGE",
        }
    }
}

/// Split `user.max` into the metric name and its consolidation function.
pub fn metric_and_cf(token: &str) -> (&str, Cf) {
    if let Some((name, suffix)) = token.rsplit_once('.') {
        match suffix {
            "max" => return (name, Cf::Max),
            "min" => return (name, Cf::Min),
            "average" => return (name, Cf::Average),
            _ => {}
        }
    }
    (token, Cf::Max)
}

/// Whether an RPN token refers to a metric (not an operator and not a number).
fn is_variable(token: &str) -> bool {
    let Some(first) = token.chars().next() else { return false };
    !("+-/*".contains(first) || token.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// PNP4Nagios file name escaping.
pub fn pnp_cleanup(s: &str) -> String {
    s.chars().map(|c| if matches!(c, ' ' | '/' | '\\' | ':') { '_' } else { c }).collect()
}

/// Location of the RRD of a metric (PNP4Nagios "multiple" storage layout).
pub fn metric_path(pnp_dir: &Path, host: &str, service: &str, metric: &str) -> PathBuf {
    pnp_dir.join(host).join(pnp_cleanup(&format!("{service}_{}.rrd", pnp_cleanup(metric))))
}

/// An RPN expression whose metric references have been replaced by `var_1`,
/// `var_2`, ... together with the RRD to read and the consolidation function.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub tokens: Vec<String>,
    pub metric: String,
    pub cf: Cf,
}

impl Plan {
    /// Build the plan for an RPN expression. `None` if it references no metric.
    ///
    /// Like the original Livestatus, the RRD and the consolidation function of the
    /// last metric reference are used; only one metric per expression is supported.
    pub fn new(rpn: &str) -> Option<Plan> {
        let mut tokens = Vec::new();
        let mut last: Option<(&str, Cf)> = None;
        let mut variables = 0;
        for token in rpn.split(',') {
            if is_variable(token) {
                variables += 1;
                last = Some(metric_and_cf(token));
                tokens.push(format!("var_{variables}"));
            } else {
                tokens.push(token.to_string());
            }
        }
        let (metric, cf) = last?;
        Some(Plan { tokens, metric: metric.to_string(), cf })
    }

    /// Compute the expression for one data point (`var_1` = `value`).
    pub fn apply(&self, value: f64) -> Result<f64, String> {
        let mut stack: Vec<f64> = Vec::with_capacity(4);
        for token in &self.tokens {
            if token == "var_1" {
                stack.push(value);
                continue;
            }
            if let Ok(number) = token.parse::<f64>() {
                stack.push(number);
                continue;
            }
            let (Some(rhs), Some(lhs)) = (stack.pop(), stack.pop()) else {
                return Err("invalid rpn".into());
            };
            stack.push(match token.as_str() {
                "+" => lhs + rhs,
                "-" => lhs - rhs,
                "*" => lhs * rhs,
                "/" => lhs / rhs,
                op => return Err(format!("unsupported operation {op}")),
            });
        }
        match stack.as_slice() {
            [result] => Ok(*result),
            _ => Err("invalid rpn".into()),
        }
    }
}

/// Consolidate `values` (one per `step` seconds) to the target resolution by
/// combining `target / step` consecutive values. Missing values (NaN) are ignored;
/// an incomplete trailing group is dropped. Returns the values and the new step.
pub fn consolidate(cf: Cf, values: &[f64], step: u64, target: u64) -> (Vec<f64>, u64) {
    if step == 0 || step >= target {
        return (values.to_vec(), step);
    }
    let factor = (target / step) as usize;
    let out = values
        .chunks_exact(factor)
        .map(|chunk| {
            let present = chunk.iter().copied().filter(|v| !v.is_nan());
            match cf {
                Cf::Max => present.fold(f64::NAN, f64::max),
                Cf::Min => present.fold(f64::NAN, f64::min),
                Cf::Average => {
                    let (sum, n) = present.fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
                    if n == 0 {
                        f64::NAN
                    } else {
                        sum / f64::from(n)
                    }
                }
            }
        })
        .collect();
    (out, step * factor as u64)
}

/// The answer of rrdcached to `FETCHBIN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    pub start: i64,
    pub end: i64,
    pub step: u64,
    pub values: Vec<f64>,
}

/// Read a metric through rrdcached: `FETCHBIN <file> <CF> <start> <end> <ds>`.
pub fn fetch_bin(socket: &Path, file: &Path, cf: Cf, start: i64, end: i64) -> Result<Fetched, String> {
    let stream = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| e.to_string())?;
    let request = format!("FETCHBIN {} {} {start} {end} 1\n", file.display(), cf.rrd_name());
    (&stream).write_all(request.as_bytes()).map_err(|e| format!("sending request: {e}"))?;
    read_fetch_bin_reply(&mut BufReader::new(&stream))
}

/// Parse a `FETCHBIN` reply:
///
/// ```text
/// 6 Success
/// FlushVersion: 1
/// Start: 1795129920
/// End: 1795130580
/// Step: 60
/// DSCount: 1
/// DSName-1: BinaryData 11 8 LITTLE
/// <11 * 8 bytes>
/// ```
pub fn read_fetch_bin_reply(r: &mut impl BufRead) -> Result<Fetched, String> {
    const HEADER_LINES: i64 = 5;
    let status = read_line(r)?;
    let code: i64 = status.split_whitespace().next().and_then(|c| c.parse().ok()).unwrap_or(-1);
    if code < HEADER_LINES {
        return Err(format!("rrdcached: {status}"));
    }
    let mut header = [0i64; HEADER_LINES as usize];
    for field in &mut header {
        let line = read_line(r)?;
        *field = line.split_once(": ").and_then(|(_, v)| v.trim().parse().ok()).unwrap_or(0);
    }
    let [_flush_version, start, end, step, _ds_count] = header;

    // DSName-1: BinaryData <count> <size> <LITTLE|BIG>
    let payload = read_line(r)?;
    let parts: Vec<&str> = payload.split_whitespace().collect();
    let [_, "BinaryData", count, size, endianness] = parts.as_slice() else {
        return Err(format!("invalid payload header: {payload}"));
    };
    let count: usize = count.parse().map_err(|_| format!("invalid payload header: {payload}"))?;
    if *size != "8" {
        return Err(format!("unsupported value size: {payload}"));
    }
    let mut raw = vec![0u8; count.checked_mul(8).ok_or("payload too large")?];
    r.read_exact(&mut raw).map_err(|e| format!("reading payload: {e}"))?;
    let big_endian = endianness.eq_ignore_ascii_case("BIG");
    let values = raw
        .chunks_exact(8)
        .map(|b| {
            let bytes: [u8; 8] = b.try_into().expect("chunk of 8 bytes");
            if big_endian {
                f64::from_be_bytes(bytes)
            } else {
                f64::from_le_bytes(bytes)
            }
        })
        .collect();
    Ok(Fetched { start, end, step: u64::try_from(step).unwrap_or(0), values })
}

fn read_line(r: &mut impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    match r.read_line(&mut line) {
        Ok(0) => Err("rrdcached closed the connection".into()),
        Ok(_) => Ok(line.trim_end().to_string()),
        Err(e) => Err(format!("reading reply: {e}")),
    }
}

/// The value an `rrddata` cell holds while the world lock is held.
pub fn object_key(host: &str, service: &str) -> Value {
    Value::List(vec![Value::str(host), Value::str(service)])
}

/// Compute the `rrddata` value for one object. Errors (missing RRD, rrdcached not
/// reachable, invalid expression) yield an empty list, like the original Livestatus.
pub fn rrd_data(paths: &Paths, args: &RrdArgs, host: &str, service: &str) -> Value {
    match try_rrd_data(paths, args, host, service) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("rrddata for {host};{service} ({}): {e}", args.rpn);
            Value::List(Vec::new())
        }
    }
}

fn try_rrd_data(paths: &Paths, args: &RrdArgs, host: &str, service: &str) -> Result<Value, String> {
    let plan = Plan::new(&args.rpn).ok_or("expression references no metric")?;
    let pnp = paths.pnp_path.as_deref().ok_or("no pnp_path configured")?;
    let socket = paths.rrdcached_socket.as_deref().ok_or("no rrdcached socket")?;
    // rrdcached identifies files by their real path (OMD's /omd is a symlink).
    let pnp = std::fs::canonicalize(pnp).unwrap_or_else(|_| pnp.to_path_buf());
    let file = metric_path(&pnp, host, service, &plan.metric);
    let fetched = fetch_bin(socket, &file, plan.cf, args.start, args.end)?;
    let values = fetched.values.iter().map(|&v| plan.apply(v)).collect::<Result<Vec<f64>, String>>()?;
    let (values, step) = consolidate(plan.cf, &values, fetched.step, args.resolution);
    let mut out = Vec::with_capacity(values.len() + 3);
    out.push(Value::Int(fetched.start));
    out.push(Value::Int(fetched.end));
    out.push(Value::Int(i64::try_from(step).unwrap_or(i64::MAX)));
    out.extend(values.into_iter().map(Value::Float));
    Ok(Value::List(out))
}

/// Replace the object keys in the `rrddata` columns of `rows` by the data.
pub fn resolve(paths: &Paths, columns: &[crate::tables::Column], rows: &mut [Vec<Value>]) {
    for (i, col) in columns.iter().enumerate() {
        let Some(args) = &col.rrd else { continue };
        for row in rows.iter_mut() {
            let Some(cell) = row.get_mut(i) else { continue };
            if let Value::List(key) = cell {
                if let [Value::Str(host), Value::Str(service)] = key.as_slice() {
                    *cell = rrd_data(paths, args, host, service);
                    continue;
                }
            }
            *cell = Value::List(Vec::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arguments() {
        assert_eq!(
            RrdArgs::parse("fs_used,1024,/:1426411073:1426416473:5"),
            Ok(RrdArgs {
                rpn: "fs_used,1024,/".into(),
                start: 1426411073,
                end: 1426416473,
                resolution: 5,
                max_entries: 400
            })
        );
        let a = RrdArgs::parse("util.max:1:2:60:200").unwrap();
        assert_eq!((a.resolution, a.max_entries), (60, 200));
        assert_eq!(RrdArgs::parse("util:1:2:60:").unwrap().max_entries, 400);
        assert_eq!(RrdArgs::parse("util:1795126680.0:1795130280.5:60").unwrap().end, 1795130280);
        assert_eq!(RrdArgs::parse("util:1:2"), Err("missing resolution".into()));
        assert_eq!(RrdArgs::parse(":1:2:3"), Err("missing RPN expression".into()));
        assert_eq!(RrdArgs::parse("util:x:2:3"), Err("invalid number for start time".into()));
        assert_eq!(RrdArgs::parse("util:1:2:3:4:5"), Err("too many arguments".into()));
    }

    #[test]
    fn consolidation_function_suffix() {
        assert_eq!(metric_and_cf("util.average"), ("util", Cf::Average));
        assert_eq!(metric_and_cf("user.min"), ("user", Cf::Min));
        assert_eq!(metric_and_cf("user.max"), ("user", Cf::Max));
        assert_eq!(metric_and_cf("user"), ("user", Cf::Max));
        assert_eq!(metric_and_cf("if.in.bps"), ("if.in.bps", Cf::Max));
    }

    #[test]
    fn rrd_file_location() {
        let p = metric_path(Path::new("/pnp"), "web01", "Filesystem /var/log", "fs_used");
        assert_eq!(p, PathBuf::from("/pnp/web01/Filesystem__var_log_fs_used.rrd"));
        let p = metric_path(Path::new("/pnp"), "web01", HOST_SERVICE, "rta");
        assert_eq!(p, PathBuf::from("/pnp/web01/_HOST__rta.rrd"));
        assert_eq!(pnp_cleanup(r"a b/c\d:e"), "a_b_c_d_e");
    }

    #[test]
    fn rpn_evaluation() {
        let plan = Plan::new("fs_used.average,1024,/").unwrap();
        assert_eq!(plan.metric, "fs_used");
        assert_eq!(plan.cf, Cf::Average);
        assert_eq!(plan.apply(2048.0), Ok(2.0));
        let plan = Plan::new("util.max,1.000000,*").unwrap();
        assert_eq!(plan.apply(42.5), Ok(42.5));
        assert!(plan.apply(f64::NAN).unwrap().is_nan());
        assert_eq!(Plan::new("100,util,-").unwrap().apply(30.0), Ok(70.0));
        assert_eq!(Plan::new("1,2,+"), None);
        assert!(Plan::new("a,b,+").unwrap().apply(1.0).is_err());
        assert!(Plan::new("util,+").unwrap().apply(1.0).is_err());
        assert!(Plan::new("util,2,%").unwrap().apply(1.0).is_err());
    }

    /// Render values for comparison (NaN never equals NaN).
    fn show(values: &[f64]) -> Vec<String> {
        values.iter().map(|v| format!("{v}")).collect()
    }

    #[test]
    fn consolidation() {
        let v = [1.0, 5.0, f64::NAN, 2.0, f64::NAN, f64::NAN, 7.0];
        for target in [60, 30] {
            let (out, step) = consolidate(Cf::Max, &v, 60, target);
            assert_eq!((show(&out), step), (show(&v), 60));
        }
        let (out, step) = consolidate(Cf::Max, &v, 60, 120);
        assert_eq!((show(&out), step), (show(&[5.0, 2.0, f64::NAN]), 120));
        assert_eq!(show(&consolidate(Cf::Min, &v, 60, 120).0), show(&[1.0, 2.0, f64::NAN]));
        assert_eq!(consolidate(Cf::Average, &v, 60, 180).0, vec![3.0, 2.0]);
        // non-divisible target: the factor is rounded down
        assert_eq!(consolidate(Cf::Max, &[1.0, 2.0, 3.0], 60, 100), (vec![1.0, 2.0, 3.0], 60));
    }

    fn reply(values: &[f64], endianness: &str) -> Vec<u8> {
        let mut out = format!(
            "6 Success\nFlushVersion: 1\nStart: 1000\nEnd: 1180\nStep: 60\nDSCount: 1\nDSName-1: BinaryData {} 8 {endianness}\n",
            values.len()
        )
        .into_bytes();
        for v in values {
            out.extend_from_slice(&if endianness == "BIG" { v.to_be_bytes() } else { v.to_le_bytes() });
        }
        out
    }

    #[test]
    fn parses_fetch_bin_reply() {
        for endianness in ["LITTLE", "BIG"] {
            let f = read_fetch_bin_reply(&mut &reply(&[1.5, f64::NAN, 3.0], endianness)[..]).unwrap();
            assert_eq!((f.start, f.end, f.step), (1000, 1180, 60));
            assert_eq!(f.values[0], 1.5);
            assert!(f.values[1].is_nan());
            assert_eq!(f.values[2], 3.0);
        }
        let err =
            read_fetch_bin_reply(&mut &b"-1 rrd_fetch_r failed: opening '/x.rrd': No such file or directory\n"[..])
                .unwrap_err();
        assert!(err.contains("No such file"), "{err}");
        let mut truncated = reply(&[1.0, 2.0], "LITTLE");
        truncated.truncate(truncated.len() - 3);
        assert!(read_fetch_bin_reply(&mut &truncated[..]).is_err());
    }
}
