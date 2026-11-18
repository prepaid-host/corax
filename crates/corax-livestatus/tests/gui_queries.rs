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

/// Shared state whose `nagios.log` is the unmodified history of a real Nagios site.
fn shared_with_reference_history(dir: &Path) -> Arc<Shared> {
    let log_file = dir.join("nagios.log");
    std::fs::copy(local_fixture("statehist-nagios.log"), &log_file).unwrap();
    let paths = Paths { log_file: Some(log_file), log_archive_path: Some(dir.join("archive")), ..Default::default() };
    let (shared, rx) = Shared::new(build_world(), paths);
    std::mem::forget(rx);
    shared
}

fn sorted_rows(json: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    let mut rows: Vec<String> = v.as_array().unwrap().iter().map(|r| r.to_string()).collect();
    rows.sort();
    rows
}

/// The availability queries of the Checkmk GUI yield exactly what Nagios' Livestatus
/// computed from the same history (`statehist-samples.json`).
#[test]
fn statehist_matches_reference_nagios() {
    let dir = tempfile::tempdir().unwrap();
    let s = shared_with_reference_history(dir.path());
    let samples: Vec<serde_json::Value> =
        serde_json::Deserializer::from_str(&std::fs::read_to_string(local_fixture("statehist-samples.json")).unwrap())
            .into_iter()
            .map(Result::unwrap)
            .collect();
    assert_eq!(samples.len(), 2);

    let hosts = run(
        &s,
        "GET statehist\nColumns: host_name service_description duration from until state host_down in_downtime \
         in_host_downtime in_notification_period in_service_period is_flapping\nFilter: time >= 1794960000\n\
         Filter: time < 1795008912\nFilter: service_description =\nTimelimit: 30\nOutputFormat: json\n\n",
    );
    assert_eq!(sorted_rows(&hosts), sorted_rows(&samples[0].to_string()));

    let svc = run(
        &s,
        "GET statehist\nColumns: host_name service_description from until duration duration_part state log_output\n\
         Filter: time >= 1794960000\nFilter: time < 1795008912\nFilter: service_description = Memory\n\
         Filter: host_name = db01\nOutputFormat: json\n\n",
    );
    let got: serde_json::Value = serde_json::from_str(&svc).unwrap();
    let want = &samples[1];
    assert_eq!(got.as_array().unwrap().len(), want.as_array().unwrap().len(), "{svc}");
    for (g, w) in got.as_array().unwrap().iter().zip(want.as_array().unwrap()) {
        for i in [0, 1, 2, 3, 4, 6, 7] {
            assert_eq!(g[i], w[i], "{svc}");
        }
        assert!((g[5].as_f64().unwrap() - w[5].as_f64().unwrap()).abs() < 1e-5, "{svc}");
    }
}

#[test]
fn statehist_stats_and_auth() {
    let dir = tempfile::tempdir().unwrap();
    let s = shared_with_reference_history(dir.path());
    // availability summary per host: sum of durations covers the whole period
    let out = run(
        &s,
        "GET statehist\nColumns: host_name\nFilter: time >= 1794960000\nFilter: time < 1795008912\n\
         Filter: service_description =\nStats: sum duration\nStats: sum duration_ok\nStats: sum duration_unmonitored\n\
         OutputFormat: python3\n\n",
    );
    assert_eq!(
        out,
        "[[\"db01\",48911,5093,43818],\n[\"localhost\",48911,5093,43818],\n[\"web01\",48911,5093,43818]]\n"
    );
    let none = run(&s, "GET statehist\nColumns: host_name\nFilter: time >= 1794960000\nAuthUser: nosuchuser\n\n");
    assert_eq!(none, "");
    // no time filter: the period ends now and starts at the epoch
    let all = run(&s, "GET statehist\nColumns: state\nStats: state >= -1\nOutputFormat: python3\n\n");
    assert_ne!(all, "[[0]]\n");
}

/// `pnpgraph_present` is answered from a directory listing taken before the world lock;
/// output columns, filters, stats and joined `host_` columns must all see it.
#[test]
fn pnpgraph_present_from_directory_index() {
    let dir = tempfile::tempdir().unwrap();
    let pnp = dir.path().join("perfdata");
    std::fs::create_dir_all(pnp.join("db01")).unwrap();
    std::fs::write(pnp.join("db01/_HOST_.xml"), "").unwrap();
    std::fs::write(pnp.join("db01/Memory.xml"), "").unwrap();
    std::fs::write(pnp.join("db01/CPU_utilization.rrd"), "").unwrap();
    std::fs::create_dir_all(pnp.join("db01/Uptime.xml")).unwrap(); // a directory is no graph
    let log_file = dir.path().join("nagios.log");
    std::fs::copy(local_fixture("nagios.log"), &log_file).unwrap();
    let paths = Paths { log_file: Some(log_file), pnp_path: Some(pnp.clone()), ..Default::default() };
    let (s, rx) = Shared::new(build_world(), paths);
    std::mem::forget(rx);

    let hosts = run(&s, "GET hosts\nColumns: name pnpgraph_present\nFilter: name = db01\n\n");
    assert_eq!(hosts, "db01;1\n");
    let services = run(
        &s,
        "GET services\nColumns: description pnpgraph_present host_pnpgraph_present\nFilter: host_name = db01\n\
         Filter: description = Memory\nFilter: description = Uptime\nOr: 2\n\n",
    );
    assert_eq!(sorted_lines(&services), ["Memory;1;1", "Uptime;0;1"]);
    let filtered = run(&s, "GET services\nColumns: host_name description\nFilter: pnpgraph_present = 1\n\n");
    assert_eq!(filtered, "db01;Memory\n");
    let stats = run(&s, "GET services\nStats: pnpgraph_present = 1\n\n");
    assert_eq!(stats, "1\n");

    // The cached listing notices graphs created later.
    std::fs::write(pnp.join("db01/CPU_utilization.xml"), "").unwrap();
    let stats = run(&s, "GET services\nStats: pnpgraph_present = 1\n\n");
    assert_eq!(stats, "2\n");
}

fn sorted_lines(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.lines().collect();
    v.sort();
    v
}
