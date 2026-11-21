//! Runs real Livestatus queries captured from the Checkmk 2.5 GUI against a world
//! built from the object configuration of a real Checkmk site.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use corax_core::{Comment, Downtime, Paths, Shared, Target, World};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/checkmk")
}

fn local_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn build_world() -> World {
    let files: Vec<PathBuf> = ["templates.cfg", "check_mk_templates.cfg", "check_mk_objects.cfg"]
        .iter()
        .map(|f| fixture_dir().join(f))
        .collect();
    let (objects, _) = corax_config::load_objects(&files).expect("fixture config");
    let mut resources = corax_config::Resources::default();
    resources.parse_into("$USER1$=/omd/sites/site/lib/nagios/plugins\n$USER4$=/omd/sites/site\n");
    let cfg = corax_config::LoadedConfig { main: Default::default(), resources, objects, warnings: vec![] };
    let mut world = World::new(&cfg, 1_795_003_800);

    let now = 1_795_003_900;
    for h in &mut world.hosts {
        h.st.has_been_checked = true;
        h.st.last_check = now;
        h.st.plugin_output = "OK - 127.0.0.1 rta 0.012ms lost 0%".into();
        h.st.perf_data = "rta=0.012ms;200.000;500.000;0; pl=0%;80;100;0;100".into();
    }
    for (i, s) in world.services.iter_mut().enumerate() {
        s.st.has_been_checked = i % 5 != 0;
        s.st.current_state = (i % 4) as u8;
        s.st.last_hard_state = s.st.current_state;
        s.st.plugin_output = format!("Output {i} with \"quotes\" and ümlaut");
        s.st.long_plugin_output = "line1\nline2".into();
        s.st.perf_data = "util=24.75;;;0; 'a b'=1".into();
        s.st.last_check = now;
    }
    let db01 = world.find_host("db01").unwrap();
    let mem = world.find_service("db01", "Memory").unwrap();
    world.service_mut(mem).st.acknowledged = true;
    world.comments.insert(
        1,
        Comment {
            id: 1,
            target: Target::Host(db01),
            entry_type: 1,
            author: "cmkadmin".into(),
            text: "hello".into(),
            entry_time: now,
            persistent: false,
            source: 1,
            expires: false,
            expire_time: 0,
        },
    );
    world.downtimes.insert(
        1,
        Downtime {
            id: 1,
            target: Target::Service(mem),
            author: "cmkadmin".into(),
            comment: "maint".into(),
            entry_time: now,
            start_time: now - 10,
            end_time: now + 3600,
            fixed: true,
            duration: 3610,
            triggered_by: 0,
            is_in_effect: true,
            flex_downtime_start: 0,
            comment_id: 0,
        },
    );
    world.service_mut(mem).st.scheduled_downtime_depth = 1;
    world
}

fn shared(log_dir: &Path) -> Arc<Shared> {
    let log_file = log_dir.join("nagios.log");
    std::fs::copy(local_fixture("nagios.log"), &log_file).unwrap();
    let paths =
        Paths { log_file: Some(log_file), log_archive_path: Some(log_dir.join("archive")), ..Default::default() };
    let (shared, rx) = Shared::new(build_world(), paths);
    std::mem::forget(rx); // keep the command channel open
    shared
}

fn queries() -> Vec<String> {
    std::fs::read_to_string(local_fixture("gui-queries.txt"))
        .unwrap()
        .split("\n\n")
        .filter(|q| !q.trim().is_empty())
        .map(|q| format!("{q}\n"))
        .collect()
}

fn python_parses(body: &[u8]) -> bool {
    let Ok(mut child) = Command::new("python3")
        .args(["-c", "import ast,sys; ast.literal_eval(sys.stdin.read())"])
        .stdin(std::process::Stdio::piped())
        .spawn()
    else {
        return true; // python not available: skip
    };
    use std::io::Write;
    child.stdin.take().unwrap().write_all(body).unwrap();
    child.wait().map(|s| s.success()).unwrap_or(true)
}

#[test]
fn all_captured_gui_queries_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let shared = shared(dir.path());
    let qs = queries();
    assert!(qs.len() >= 30);
    for q in qs {
        let resp = corax_livestatus::execute(&shared, &q);
        let body = String::from_utf8_lossy(&resp.body).into_owned();
        assert_eq!(resp.status, 200, "query failed:\n{q}\n=> {body}");
        assert!(resp.fixed16 && resp.keepalive);
        let bytes = resp.to_bytes();
        let header = std::str::from_utf8(&bytes[..16]).unwrap();
        assert_eq!(header[4..15].trim().parse::<usize>().unwrap(), resp.body.len());
        if q.contains("OutputFormat: json") {
            serde_json::from_slice::<serde_json::Value>(&resp.body)
                .unwrap_or_else(|e| panic!("bad json for\n{q}: {e}\n{body}"));
        } else if q.contains("OutputFormat: python") {
            assert!(python_parses(&resp.body), "bad python for\n{q}\n{body}");
        }
    }
}

fn run(shared: &Shared, q: &str) -> String {
    let r = corax_livestatus::execute(shared, q);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    String::from_utf8(r.body).unwrap()
}

#[test]
fn hosts_labels_and_custom_variables() {
    let dir = tempfile::tempdir().unwrap();
    let s = shared(dir.path());
    let out = run(&s, "GET hosts\nColumns: name labels tags filename custom_variable_names\nFilter: name = db01\nOutputFormat: json\n\n");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let row = &v[0];
    assert_eq!(row[0], "db01");
    assert_eq!(row[1]["cmk/os_family"], "linux");
    assert_eq!(row[2]["site"], "cmk");
    assert_eq!(row[3], "/wato/hosts.mk");
    let names: Vec<&str> = row[4].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert!(names.contains(&"TAGS") && names.contains(&"FILENAME"));
    assert!(names.iter().all(|n| !n.starts_with("_LABEL") && !n.starts_with("_TAG")));

    // dict filter on labels
    let out = run(&s, "GET hosts\nColumns: name\nFilter: labels = cmk/os_family linux\nOutputFormat: python3\n\n");
    assert_eq!(out.matches("\"").count(), 6, "{out}");
}

#[test]
fn stats_and_grouping() {
    let dir = tempfile::tempdir().unwrap();
    let s = shared(dir.path());
    let total = run(&s, "GET services\nStats: state >= 0\nOutputFormat: python3\n\n");
    let n = s.world.read().services.len();
    assert_eq!(total, format!("[[{n}]]\n"));
    let grouped = run(
        &s,
        "GET services\nColumns: host_name\nStats: state >= 0\nStats: avg execution_time\nOutputFormat: python3\n\n",
    );
    assert!(grouped.starts_with("[[\"db01\","), "{grouped}");
    assert_eq!(grouped.lines().count(), 3);
    let empty = run(&s, "GET hosts\nStats: avg latency\nStats: min state\nStats: sum check_interval\nFilter: state = 9\nOutputFormat: python3\n\n");
    assert_eq!(empty, "[[None,0,0]]\n");
}

#[test]
fn joins_comments_downtimes_and_log() {
    let dir = tempfile::tempdir().unwrap();
    let s = shared(dir.path());
    let c = run(&s, "GET comments\nColumns: id type author comment host_name service_description service_state\nOutputFormat: python3\n\n");
    assert_eq!(c, "[[1,1,\"cmkadmin\",\"hello\",\"db01\",\"\",0]]\n");
    let d =
        run(&s, "GET downtimes\nColumns: id type is_pending host_name service_description\nOutputFormat: python3\n\n");
    assert_eq!(d, "[[1,1,0,\"db01\",\"Memory\"]]\n");
    let log = run(&s, "GET log\nColumns: time class type host_name state_info\nFilter: class = 1\nLimit: 2\nOutputFormat: python3\n\n");
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(log.contains("ALERT"), "{log}");
}

#[test]
fn auth_user_restricts_rows() {
    let dir = tempfile::tempdir().unwrap();
    let s = shared(dir.path());
    let all = run(&s, "GET hosts\nColumns: name\n\n");
    assert_eq!(all.lines().count(), 3);
    let nobody = run(&s, "GET hosts\nColumns: name\nAuthUser: nosuchuser\n\n");
    assert_eq!(nobody, "");
    let notify = run(&s, "GET hosts\nColumns: name\nAuthUser: check-mk-notify\n\n");
    assert_eq!(notify.lines().count(), 3);
}

#[test]
fn errors_match_livestatus() {
    let dir = tempfile::tempdir().unwrap();
    let s = shared(dir.path());
    let r = corax_livestatus::execute(&s, "GET foo\nResponseHeader: fixed16\n\n");
    assert_eq!(String::from_utf8(r.to_bytes()).unwrap(), "404          41\nInvalid GET request, no such table 'foo'\n");
    let r = corax_livestatus::execute(&s, "GET hosts\nFoo: bar\nResponseHeader: fixed16\n\n");
    assert_eq!(
        String::from_utf8(r.to_bytes()).unwrap(),
        "400          56\nwhile processing header 'Foo': undefined request header\n"
    );
}

#[tokio::test]
async fn socket_roundtrip_with_keepalive_and_commands() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { ..Default::default() };
    let (shared, mut rx) = Shared::new(build_world(), paths);
    let sock = dir.path().join("live");
    let server = tokio::spawn(corax_livestatus::serve(shared.clone(), sock.clone()));
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut c = tokio::net::UnixStream::connect(&sock).await.unwrap();
    c.write_all(b"GET status\nColumns: num_hosts\nKeepAlive: on\nResponseHeader: fixed16\n\n").await.unwrap();
    let mut header = [0u8; 16];
    c.read_exact(&mut header).await.unwrap();
    assert_eq!(&header, b"200           2\n");
    let mut body = [0u8; 2];
    c.read_exact(&mut body).await.unwrap();
    assert_eq!(&body, b"3\n");
    c.write_all(b"COMMAND [1795003860] DISABLE_NOTIFICATIONS\n\nGET hosts\nColumns: name\nFilter: name = web01\n\n")
        .await
        .unwrap();
    let mut rest = String::new();
    c.read_to_string(&mut rest).await.unwrap();
    assert_eq!(rest, "web01\n");
    match rx.recv().await {
        Some(corax_core::Input::ExternalCommand { line, .. }) => assert_eq!(line, "[1795003860] DISABLE_NOTIFICATIONS"),
        other => panic!("unexpected {other:?}"),
    }
    server.abort();
}

/// A minimal rrdcached: answers `FETCHBIN` for the given files and records requests.
fn fake_rrdcached(socket: &Path, files: Vec<(PathBuf, Vec<f64>)>) -> Arc<std::sync::Mutex<Vec<String>>> {
    use std::io::{BufRead, BufReader, Write};
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let line = line.trim_end().to_string();
            seen.lock().unwrap().push(line.clone());
            let file = line.split_whitespace().nth(1).map(PathBuf::from).unwrap_or_default();
            let reply = match files.iter().find(|(f, _)| *f == file) {
                Some((_, values)) => {
                    let mut out = format!(
                        "6 Success\nFlushVersion: 1\nStart: 1000\nEnd: 1240\nStep: 60\nDSCount: 1\nDSName-1: BinaryData {} 8 LITTLE\n",
                        values.len()
                    )
                    .into_bytes();
                    values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
                    out
                }
                None => format!("-1 rrd_fetch_r failed: opening '{}': No such file or directory\n", file.display())
                    .into_bytes(),
            };
            stream.write_all(&reply).unwrap();
        }
    });
    requests
}

/// A site with PNP4Nagios RRDs for `db01` served by a fake rrdcached.
fn rrd_site(dir: &Path) -> (Arc<Shared>, Arc<std::sync::Mutex<Vec<String>>>) {
    let pnp = dir.join("perfdata");
    std::fs::create_dir_all(pnp.join("db01")).unwrap();
    // rrdcached is addressed with the real path (OMD's /omd is a symlink)
    let real = std::fs::canonicalize(&pnp).unwrap().join("db01");
    let socket = dir.join("rrdcached.sock");
    let requests = fake_rrdcached(
        &socket,
        vec![
            (real.join("CPU_utilization_util.rrd"), vec![1.0, 3.0, f64::NAN, 4.0]),
            (real.join("_HOST__rta.rrd"), vec![0.5, 0.25]),
        ],
    );
    let paths = Paths { pnp_path: Some(pnp), rrdcached_socket: Some(socket), ..Default::default() };
    let (shared, rx) = Shared::new(build_world(), paths);
    std::mem::forget(rx);
    (shared, requests)
}

#[test]
fn rrddata_reads_metrics_through_rrdcached() {
    let dir = tempfile::tempdir().unwrap();
    let (s, requests) = rrd_site(dir.path());
    let svc = "Filter: host_name = db01\nFilter: description = CPU utilization\nOutputFormat: python3\n\n";

    // the query the Checkmk GUI sends for a graph (metric with scaling factor)
    let out =
        run(&s, &format!("GET services\nColumns: host_name rrddata:util:util.max,2.000000,*:1000:1240:60\n{svc}"));
    assert_eq!(out, "[[\"db01\",[1000,1240,60,2,6,None,8]]]\n");
    let real = std::fs::canonicalize(dir.path().join("perfdata")).unwrap();
    assert_eq!(
        requests.lock().unwrap().last().unwrap(),
        &format!("FETCHBIN {}/db01/CPU_utilization_util.rrd MAX 1000 1240 1", real.display())
    );

    // consolidation to a coarser resolution, CF from the metric suffix
    let out = run(&s, &format!("GET services\nColumns: rrddata:util:util.average:1000:1240:120\n{svc}"));
    assert_eq!(out, "[[[1000,1240,120,2,4]]]\n");
    assert!(requests.lock().unwrap().last().unwrap().contains(" AVERAGE 1000 1240 1"));

    // host metrics: rrddata of the hosts table and host_rrddata of the services table
    let out =
        run(&s, "GET hosts\nColumns: rrddata:rta:rta:1000:1240:60\nFilter: name = db01\nOutputFormat: python3\n\n");
    assert_eq!(out, "[[[1000,1240,60,0.5,0.25]]]\n");
    let out = run(&s, &format!("GET services\nColumns: host_rrddata:rta:rta,1000,*:1000:1240:60\n{svc}"));
    assert_eq!(out, "[[[1000,1240,60,500,250]]]\n");

    // the title is the column header; JSON renders missing values as null
    let out = run(
        &s,
        "GET services\nColumns: rrddata:util:util:1000:1240:60\nColumnHeaders: on\nFilter: host_name = db01\nFilter: description = CPU utilization\nOutputFormat: json\n\n",
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0][0], "util");
    assert_eq!(v[1][0], serde_json::json!([1000, 1240, 60, 1, 3, null, 4]));

    // metrics without an RRD yield an empty list
    let out = run(&s, &format!("GET services\nColumns: rrddata:x:nosuchmetric:1000:1240:60\n{svc}"));
    assert_eq!(out, "[[[]]]\n");
}

#[test]
fn rrddata_without_rrdcached_or_with_bad_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let s = shared(dir.path());
    let out = run(
        &s,
        "GET hosts\nColumns: name rrddata:rta:rta:1000:1240:60\nFilter: name = db01\nOutputFormat: python3\n\n",
    );
    assert_eq!(out, "[[\"db01\",[]]]\n");

    for (query, error) in [
        (
            "GET hosts\nColumns: rrddata:rta:rta:1000:1240\n\n",
            "invalid arguments for column 'rrddata': missing resolution",
        ),
        ("GET hosts\nColumns: rrddata::rta:1:2:3\n\n", "empty column name for dynamic column 'rrddata'"),
        ("GET hosts\nColumns: rrddata:rta\n\n", "missing separator in dynamic column 'rrddata'"),
        ("GET contacts\nColumns: rrddata:a:b:1:2:3\n\n", "table 'contacts' has no dynamic column 'rrddata'"),
        ("GET hosts\nFilter: rrddata:rta:rta:1:2:3 = 1\n\n", "has no column"),
    ] {
        let r = corax_livestatus::execute(&s, query);
        let body = String::from_utf8_lossy(&r.body);
        assert_eq!(r.status, 400, "{query}=> {body}");
        assert!(body.contains(error), "{query}=> {body}");
    }
}

/// A site whose `nagios.log` holds the given lines (`+N text` becomes `[T+N] text`).
fn statehist_site(dir: &Path, lines: &str) -> Arc<Shared> {
    const T: i64 = 1_795_000_000;
    let text: String = lines
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (offset, rest) = l.split_once(' ').unwrap();
            format!("[{}] {rest}\n", T + offset.parse::<i64>().unwrap())
        })
        .collect();
    let log_file = dir.join("nagios.log");
    std::fs::write(&log_file, text).unwrap();
    let paths = Paths { log_file: Some(log_file), log_archive_path: Some(dir.join("archive")), ..Default::default() };
    let (shared, rx) = Shared::new(build_world(), paths);
    std::mem::forget(rx);
    shared
}

#[test]
fn statehist_rebuilds_the_history_from_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let s = statehist_site(
        dir.path(),
        "0 corax 0.3.0 starting... (PID=1)
         0 LOG VERSION: 2.0
         0 TIMEPERIOD TRANSITION: 24X7;-1;1
         0 logging initial states
         0 INITIAL HOST STATE: db01;UP;HARD;1;up
         0 INITIAL HOST STATE: web01;UP;HARD;1;up
         0 INITIAL SERVICE STATE: db01;Memory;OK;HARD;1;fine
         0 INITIAL SERVICE STATE: web01;Memory;OK;HARD;1;fine
         0 INITIAL SERVICE STATE: web01;No longer configured;OK;HARD;1;fine
         250 SERVICE ALERT: db01;Memory;CRITICAL;HARD;1;full
         750 SERVICE ALERT: db01;Memory;OK;HARD;1;fine again",
    );
    let period = "Filter: time >= 1795000000\nFilter: time < 1795001000\n";
    let q = |rest: &str| run(&s, &format!("GET statehist\n{period}{rest}\n"));

    let memory =
        q("Columns: host_name service_description from until duration state in_notification_period log_output\n\
         Filter: host_name = db01\nFilter: service_description = Memory\nOutputFormat: python3\n");
    assert_eq!(
        memory,
        "[[\"db01\",\"Memory\",1795000000,1795000250,250,0,1,\"fine\"],\n\
         [\"db01\",\"Memory\",1795000250,1795000750,500,2,1,\"full\"],\n\
         [\"db01\",\"Memory\",1795000750,1795000999,249,0,1,\"fine again\"]]\n"
    );
    // one row per host and unchanged service, three for the service that changed
    assert_eq!(q("Stats: state >= -1\n"), "6\n");
    assert_eq!(q("Columns: state\nFilter: service_description = Memory\nStats: sum duration\n"), "0;1498\n2;500\n");
    assert_eq!(q("Stats: sum duration_part_critical\nFilter: host_name = db01\n"), "0.5\n");
    // columns of the current host and service, with and without the `current_` prefix
    assert_eq!(
        q("Columns: current_host_name service_description host_address\n\
           Filter: current_service_description = Memory\nFilter: host_name = web01\n"),
        "web01;Memory;127.0.0.1\n"
    );
    assert_eq!(q("Columns: host_name\nFilter: service_description = \nAuthUser: check-mk-notify\n").lines().count(), 2);
    assert_eq!(q("Columns: host_name\nAuthUser: nosuchuser\n"), "");
    // outside of the logged time there is nothing to report
    assert_eq!(run(&s, "GET statehist\nColumns: host_name\nFilter: time < 1794000000\n\n"), "");
}

/// Dumps corax answers for all captured queries into `$CORAX_DUMP_DIR` (manual
/// comparison against a reference Livestatus).
#[test]
#[ignore]
fn dump_answers() {
    let Some(out) = std::env::var_os("CORAX_DUMP_DIR") else { return };
    let dir = tempfile::tempdir().unwrap();
    let s = shared(dir.path());
    let mut qs = queries();
    if let Some(extra) = std::env::var_os("CORAX_EXTRA_QUERY") {
        qs.push(std::fs::read_to_string(extra).unwrap());
    }
    for (i, q) in qs.iter().enumerate() {
        let r = corax_livestatus::execute(&s, q);
        std::fs::write(PathBuf::from(&out).join(format!("{i:03}.out")), r.body).unwrap();
    }
}
