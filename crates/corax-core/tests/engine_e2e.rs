//! End-to-end test of the engine with a Checkmk-like site layout in a temp directory:
//! active checks run, spool results are reaped, commands arrive through the FIFO,
//! and log/retention/perfdata files are written in Nagios formats.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use corax_core::engine::{Control, Engine, EngineOptions};
use corax_core::{Paths, Shared, World};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn site_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let main = root.join("nagios.cfg");
    write(
        &root.join("etc/npcd.cfg"),
        &format!(
            "perfdata_file = {r}/var/perfdata.dump\nperfdata_spool_dir = {r}/var/spool/\nperfdata_file_processing_interval = 1\n",
            r = root.display()
        ),
    );
    std::fs::create_dir_all(root.join("var/spool")).unwrap();
    std::fs::create_dir_all(root.join("tmp/checkresults")).unwrap();
    write(
        &main,
        &format!(
            "cfg_dir={r}/conf.d\nresource_file={r}/resource.cfg\nlog_file={r}/var/nagios.log\nlog_archive_path={r}/var/archive\n\
             check_result_path={r}/tmp/checkresults\ncommand_file={r}/tmp/nagios.cmd\nstate_retention_file={r}/var/retention.dat\n\
             temp_path={r}/tmp\nlog_initial_states=1\nprocess_performance_data=1\ninterval_length=1\n\
             broker_module={r}/lib/npcdmod.o config_file={r}/etc/npcd.cfg\n",
            r = root.display()
        ),
    );
    write(&root.join("resource.cfg"), "$USER1$=/bin\n");
    write(
        &root.join("conf.d/objects.cfg"),
        "define command {\n command_name echo-ok\n command_line $USER1$/echo 'OK - fine|val=1\\;\\;\\;\\;'\n}\n\
         define command {\n command_name dummy\n command_line /bin/false\n}\n\
         define host {\n host_name web01\n address 127.0.0.1\n max_check_attempts 1\n check_command echo-ok\n check_interval 1\n}\n\
         define service {\n host_name web01\n service_description Memory\n check_command dummy\n max_check_attempts 1\n active_checks_enabled 0\n}\n",
    );

    let loaded = corax_config::load(&main).unwrap();
    let paths = Paths::from_main(&loaded.main);
    assert!(paths.npcd_perfdata_file.is_some());
    let world = World::new(&loaded, corax_core::now());
    let (shared, rx) = Shared::new(world, paths.clone());
    let main2 = main.clone();
    let mut engine = Engine::new(
        shared.clone(),
        rx,
        EngineOptions { check_helper: false, max_concurrent_checks: 4 },
        Box::new(move || Ok(corax_config::load(&main2)?)),
    );
    engine.startup();
    let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(engine.run(ctl_rx));
    let pipe_shared = shared.clone();
    let cmd_file = paths.command_file.clone().unwrap();
    tokio::spawn(async move { corax_core::cmdpipe::serve(pipe_shared, &cmd_file).await });

    // 1. the active host check runs and produces perfdata
    let s: Arc<Shared> = shared.clone();
    assert!(wait_for(|| s.world.read().hosts[0].st.has_been_checked).await, "host check did not run");
    assert_eq!(shared.world.read().hosts[0].st.plugin_output, "OK - fine");
    assert_eq!(shared.world.read().hosts[0].st.perf_data, "val=1;;;;");

    // 2. a Checkmk style spool file is reaped
    write(
        &root.join("tmp/checkresults/cabcdef"),
        "host_name=web01\nservice_description=Memory\ncheck_type=1\ncheck_options=0\nreschedule_check\nlatency=0.0\nstart_time=1.0\nfinish_time=1.0\nreturn_code=2\noutput=CRIT - RAM\\ndetails|ram=99;;;;\n\n",
    );
    write(&root.join("tmp/checkresults/cabcdef.ok"), "");
    assert!(wait_for(|| s.world.read().services[0].st.current_state == 2).await, "spool result not processed");
    assert_eq!(shared.world.read().services[0].st.long_plugin_output, "details");

    // 3. a command through the FIFO
    let fifo = paths.command_file.clone().unwrap();
    assert!(wait_for(|| fifo.exists()).await);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        writeln!(f, "[1] ACKNOWLEDGE_SVC_PROBLEM;web01;Memory;2;0;1;tester;looking").unwrap();
    }
    assert!(wait_for(|| s.world.read().services[0].st.acknowledged).await, "ack via FIFO not applied");

    // 4. shutdown writes retention and the log
    ctl_tx.send(Control::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap().unwrap();

    let log = std::fs::read_to_string(root.join("var/nagios.log")).unwrap();
    assert!(log.contains("INITIAL HOST STATE: web01;UP;HARD;1;"), "{log}");
    let marker = log.find("logging initial states").expect("marker line for the state history");
    assert!(
        log.find("LOG VERSION: 2.0").unwrap() < marker && marker < log.find("INITIAL HOST STATE").unwrap(),
        "{log}"
    );
    assert!(log.contains("SERVICE ALERT: web01;Memory;CRITICAL;HARD;1;CRIT - RAM"), "{log}");
    assert!(log.contains("EXTERNAL COMMAND: ACKNOWLEDGE_SVC_PROBLEM;web01;Memory;2;0;1;tester;looking"), "{log}");
    let retention = std::fs::read_to_string(root.join("var/retention.dat")).unwrap();
    assert!(retention.contains("problem_has_been_acknowledged=1"));
    let perf = std::fs::read_to_string(root.join("var/perfdata.dump")).unwrap_or_default()
        + &std::fs::read_dir(root.join("var/spool"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect::<String>();
    assert!(perf.contains("DATATYPE::HOSTPERFDATA\tTIMET::"), "{perf}");
    assert!(perf.contains("SERVICEDESC::Memory\tSERVICEPERFDATA::ram=99;;;;"), "{perf}");
}
