//! Manual scaling check: times a large GUI query on synthetic sites of growing size.
//! Run with `cargo test --release -p corax-livestatus --test scaling -- --ignored --nocapture`.

use std::path::PathBuf;
use std::time::Instant;

use corax_core::{Paths, Shared, World};

// Same allocator as the `corax` binary, so the numbers are representative.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/checkmk").join(name)
}

/// The fixture site with its host, service and dependency definitions copied `copies` times.
fn objects_cfg(copies: usize) -> String {
    let src = std::fs::read_to_string(fixture("check_mk_objects.cfg")).unwrap();
    let names = regex::Regex::new(r"\b(localhost|web01|db01)\b").unwrap();
    let mut parts = src.split("\ndefine ");
    let mut out = parts.next().unwrap_or("").to_string();
    for block in parts {
        let kind = block.split_whitespace().next().unwrap_or("");
        let per_host = matches!(kind, "host" | "service" | "servicedependency");
        let n = if per_host { copies } else { 1 };
        for k in 0..n {
            let b = if per_host {
                names.replace_all(block, format!("${{1}}-{k}").as_str()).into_owned()
            } else {
                block.to_string()
            };
            out.push_str("\ndefine ");
            out.push_str(&b);
        }
    }
    out
}

fn world(copies: usize, dir: &std::path::Path) -> World {
    let objects = dir.join("objects.cfg");
    std::fs::write(&objects, objects_cfg(copies)).unwrap();
    let files = vec![fixture("templates.cfg"), fixture("check_mk_templates.cfg"), objects];
    let (objects, _) = corax_config::load_objects(&files).expect("config");
    let cfg = corax_config::LoadedConfig {
        main: Default::default(),
        resources: Default::default(),
        objects,
        warnings: vec![],
    };
    let mut w = World::new(&cfg, 1_795_003_800);
    for (i, s) in w.services.iter_mut().enumerate() {
        s.st.has_been_checked = true;
        s.st.last_check = 1_795_003_900;
        s.st.plugin_output = format!("OK - output of service {i}");
        s.st.perf_data = "util=24.75;80;90;0;100 load=1.5".into();
    }
    w
}

const ALLSERVICES: &str =
    "GET services\nColumns: host_name description state plugin_output last_state_change last_check \
perf_data acknowledged scheduled_downtime_depth host_state host_has_been_checked has_been_checked labels \
comments_with_extra_info downtimes_with_extra_info custom_variable_names custom_variable_values in_notification_period \
pnpgraph_present check_command active_checks_enabled notifications_enabled is_flapping staleness\n\
OutputFormat: python3\nResponseHeader: fixed16\n\n";

#[test]
#[ignore]
fn allservices_scaling() {
    for copies in [1, 33, 133, 266] {
        let dir = tempfile::tempdir().unwrap();
        let w = world(copies, dir.path());
        let (hosts, services) = (w.hosts.len(), w.services.len());
        let (shared, rx) = Shared::new(w, Paths::default());
        std::mem::forget(rx);
        let q = std::env::var("CORAX_QUERY").unwrap_or_else(|_| ALLSERVICES.to_string());
        corax_livestatus::execute(&shared, &q);
        let t = Instant::now();
        let runs = 5;
        let mut bytes = 0;
        for _ in 0..runs {
            bytes = corax_livestatus::execute(&shared, &q).body.len();
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / runs as f64;
        println!("hosts={hosts:4} services={services:6} {ms:8.2} ms/query  {bytes} bytes");
    }
}

#[test]
#[ignore]
fn allservices_per_column() {
    let dir = tempfile::tempdir().unwrap();
    let (shared, rx) = Shared::new(world(133, dir.path()), Paths::default());
    std::mem::forget(rx);
    let cols = ALLSERVICES.lines().nth(1).unwrap().trim_start_matches("Columns: ").to_string();
    for col in cols.split_whitespace() {
        let q = format!("GET services\nColumns: {col}\nOutputFormat: python3\n\n");
        corax_livestatus::execute(&shared, &q);
        let t = Instant::now();
        for _ in 0..5 {
            corax_livestatus::execute(&shared, &q);
        }
        println!("{col:28} {:8.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 5.0);
    }
}

#[test]
#[ignore]
fn allservices_concurrent() {
    let dir = tempfile::tempdir().unwrap();
    let (shared, rx) = Shared::new(world(133, dir.path()), Paths::default());
    std::mem::forget(rx);
    for threads in [1, 4] {
        let t = Instant::now();
        let per_thread = 10;
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    for _ in 0..per_thread {
                        corax_livestatus::execute(&shared, ALLSERVICES);
                    }
                });
            }
        });
        let ms = t.elapsed().as_secs_f64() * 1000.0 / per_thread as f64;
        println!("threads={threads} {ms:8.2} ms per query (wall, per client)");
    }
}
