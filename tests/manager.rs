// SPDX-License-Identifier: BSD-2-Clause

#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use loom::{
    linux::reactor::SeqPacketConnection,
    protocol::{MessageKind, Operation, Packet, StatusCode},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "loom-manager-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("etc/loom/services")).unwrap();
        fs::create_dir_all(root.join("run/user")).unwrap();
        let metadata = fs::metadata(&root).unwrap();
        let uid = metadata.uid();
        let gid = metadata.gid();
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(
            root.join("etc/passwd"),
            format!(
                "tester:x:{uid}:{gid}:Test:{}/home:/bin/sh\n",
                root.display()
            ),
        )
        .unwrap();
        fs::write(root.join("etc/group"), format!("tester:x:{gid}:tester\n")).unwrap();
        fs::write(
            root.join("etc/loom/loom.toml"),
            "# preserved\nschema_version = 1\ndefault_group = \"boot\"\nshutdown_group = \"shutdown\"\n[groups.boot]\nwants = [\"probe\", \"reloader\"]\n[groups.shutdown]\nwants = [\"save-state\"]\n",
        )
        .unwrap();
        fs::write(
            root.join("etc/loom/services/probe.toml"),
            "schema_version = 1\n[process]\ncommand = [\"/bin/true\"]\ntype = \"oneshot\"\n[io]\nstdout = \"null\"\nstderr = \"null\"\n",
        )
        .unwrap();
        fs::write(
            root.join("etc/loom/services/reloader.toml"),
            format!(
                "schema_version = 1\n[process]\ncommand = [\"/bin/sleep\", \"30\"]\n[actions]\nreload = [\"/usr/bin/touch\", \"{}/reloaded\"]\nstop = [\"/usr/bin/touch\", \"{}/stopped\"]\n[io]\nstdout = \"null\"\nstderr = \"null\"\n",
                root.display(),
                root.display()
            ),
        )
        .unwrap();
        fs::write(
            root.join("etc/loom/services/save-state.toml"),
            format!(
                "schema_version = 1\n[process]\ncommand = [\"/usr/bin/touch\", \"{}/saved\"]\ntype = \"oneshot\"\n[io]\nstdout = \"null\"\nstderr = \"null\"\n",
                root.display()
            ),
        )
        .unwrap();
        Self { root, child: None }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = Command::new("/bin/kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn user_manager_starts_service_and_answers_status() {
    let mut fixture = Fixture::new();
    let runtime = fixture.root.join("run/user");
    let binary = env!("CARGO_BIN_EXE_loom");
    fixture.child = Some(
        Command::new(binary)
            .args([
                "--user",
                "--root",
                fixture.root.to_str().unwrap(),
                "--config-home",
                fixture.root.join("etc").to_str().unwrap(),
                "--runtime-dir",
                runtime.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );

    let socket = runtime.join("loom/control.sock");
    let deadline = Instant::now() + Duration::from_secs(2);
    let connection = loop {
        if let Ok(connection) = SeqPacketConnection::connect(&socket) {
            break connection;
        }
        assert!(Instant::now() < deadline, "manager socket did not appear");
        thread::sleep(Duration::from_millis(10));
    };
    for request_id in 1..100 {
        let response = request(&connection, request_id, Operation::Status, b"probe");
        assert_eq!(response.status, StatusCode::Ok);
        if String::from_utf8(response.payload)
            .unwrap()
            .contains("probe\tactive")
        {
            let timings = request(&connection, 200, Operation::Timings, b"");
            assert_eq!(timings.status, StatusCode::Ok);
            let timings = String::from_utf8(timings.payload).unwrap();
            assert!(timings.starts_with("service\tqueued_ms\tstarted_ms"));
            assert!(timings.contains("\nprobe\t"));
            let critical = request(&connection, 201, Operation::CriticalPath, b"");
            assert_eq!(critical.status, StatusCode::Ok);
            assert!(
                String::from_utf8(critical.payload)
                    .unwrap()
                    .contains("services=")
            );
            let reload = request(&connection, 202, Operation::ReloadService, b"reloader");
            assert_eq!(reload.status, StatusCode::Ok);
            assert!(fixture.root.join("reloaded").is_file());

            fs::write(
                fixture.root.join("etc/loom/services/probe.toml"),
                "schema_version = 1\n[process]\ncommand = [\"/bin/true\", \"--help\"]\ntype = \"oneshot\"\n[io]\nstdout = \"null\"\nstderr = \"null\"\n",
            )
            .unwrap();
            let response = request(&connection, 100, Operation::Apply, b"");
            assert_eq!(response.status, StatusCode::Ok);
            let response = request(&connection, 101, Operation::Status, b"probe");
            assert!(
                String::from_utf8(response.payload)
                    .unwrap()
                    .contains("\t2\n")
            );

            let response = request(&connection, 102, Operation::Disable, b"probe");
            assert_eq!(response.status, StatusCode::Ok);
            let response = request(&connection, 103, Operation::IsEnabled, b"probe");
            assert_eq!(response.status, StatusCode::ServiceFailure);
            let manager = fs::read_to_string(fixture.root.join("etc/loom/loom.toml")).unwrap();
            assert!(manager.contains("# preserved"));
            assert!(!manager.contains("\"probe\""));

            let child = fixture.child.as_mut().unwrap();
            assert_eq!(
                Command::new("/bin/kill")
                    .args(["-TERM", &child.id().to_string()])
                    .status()
                    .unwrap()
                    .code(),
                Some(0)
            );
            assert!(child.wait().unwrap().success());
            fixture.child = None;
            assert!(fixture.root.join("saved").is_file());
            assert!(fixture.root.join("stopped").is_file());
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("probe did not become active");
}

fn request(
    connection: &SeqPacketConnection,
    request_id: u64,
    operation: Operation,
    payload: &[u8],
) -> Packet {
    let request = Packet {
        kind: MessageKind::Request,
        request_id,
        operation,
        status: StatusCode::Ok,
        more: false,
        payload: payload.to_vec(),
    };
    connection.send(&request.encode().unwrap()).unwrap();
    Packet::decode(&connection.receive().unwrap().unwrap()).unwrap()
}

impl Fixture {
    fn write_service(&self, name: &str, argv: &[&str], extra: &str) {
        let argv = argv
            .iter()
            .map(|arg| format!("{arg:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            self.root.join(format!("etc/loom/services/{name}.toml")),
            format!("schema_version = 1\n[process]\ncommand = [{argv}]\n{extra}\n"),
        )
        .unwrap();
    }

    fn boot(&mut self, enabled: &[&str]) -> SeqPacketConnection {
        fs::write(self.root.join("etc/loom/loom.toml"), format!(
            "# preserved\nschema_version = 1\ndefault_group = \"boot\"\n[groups.boot]\nwants = {enabled:?}\n"
        )).unwrap();
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_loom"))
                .arg("--user")
                .arg("--root")
                .arg(&self.root)
                .arg("--config-home")
                .arg(self.root.join("etc"))
                .arg("--runtime-dir")
                .arg(self.root.join("run/user"))
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        self.connect()
    }

    fn connect(&self) -> SeqPacketConnection {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(connection) =
                SeqPacketConnection::connect(&self.root.join("run/user/loom/control.sock"))
            {
                return connection;
            }
            assert!(Instant::now() < deadline, "manager socket did not appear");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn send(connection: &SeqPacketConnection, operation: Operation, payload: &[u8]) {
    connection
        .send(
            &Packet {
                kind: MessageKind::Request,
                request_id: 700,
                operation,
                status: StatusCode::Ok,
                more: false,
                payload: payload.to_vec(),
            }
            .encode()
            .unwrap(),
        )
        .unwrap();
}

fn receive(connection: &SeqPacketConnection) -> Packet {
    let mut result = Packet::decode(&connection.receive().unwrap().unwrap()).unwrap();
    while result.more {
        let chunk = Packet::decode(&connection.receive().unwrap().unwrap()).unwrap();
        assert_eq!(result.request_id, chunk.request_id);
        assert_eq!(result.status, chunk.status);
        result.payload.extend_from_slice(&chunk.payload);
        result.more = chunk.more;
    }
    result
}

fn document(packet: &Packet) -> toml_edit::DocumentMut {
    String::from_utf8(packet.payload.clone())
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn slow_reload_keeps_control_responsive_and_zero_disables_helper_timeout() {
    let mut fixture = Fixture::new();
    fixture.write_service(
        "slow",
        &["/bin/sleep", "30"],
        "[actions]\nreload = [\"/bin/sleep\", \"0.3\"]\n[supervision]\nstart_timeout_ms = 0",
    );
    let connection = fixture.boot(&[]);
    assert_eq!(
        request(&connection, 1, Operation::Start, b"slow").status,
        StatusCode::Ok
    );
    send(&connection, Operation::ReloadService, b"slow");
    let status_client = fixture.connect();
    let started = Instant::now();
    assert_eq!(
        request(&status_client, 2, Operation::Status, b"slow").status,
        StatusCode::Ok
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "helper blocked status"
    );
    assert_eq!(receive(&connection).status, StatusCode::Ok);
    assert_eq!(
        request(&connection, 3, Operation::Stop, b"slow").status,
        StatusCode::Ok
    );
}

#[test]
fn helper_timeout_does_not_break_other_services() {
    let mut fixture = Fixture::new();
    fixture.write_service(
        "slow",
        &["/bin/sleep", "30"],
        "[actions]\nreload = [\"/bin/sleep\", \"30\"]\n[supervision]\nstart_timeout_ms = 100",
    );
    let connection = fixture.boot(&[]);
    assert_eq!(
        request(&connection, 1, Operation::Start, b"slow").status,
        StatusCode::Ok
    );
    assert_eq!(
        request(&connection, 2, Operation::ReloadService, b"slow").status,
        StatusCode::Timeout
    );
    assert_eq!(
        request(&connection, 3, Operation::IsActive, b"slow").status,
        StatusCode::Ok
    );
}

#[test]
fn enable_now_waits_and_reports_start_failure() {
    let mut fixture = Fixture::new();
    fixture.write_service(
        "delayed",
        &["/bin/sh", "-c", "sleep 0.15"],
        "type = \"oneshot\"",
    );
    fixture.write_service("broken", &["/bin/false"], "type = \"oneshot\"");
    let connection = fixture.boot(&[]);
    let started = Instant::now();
    assert_eq!(
        request(&connection, 1, Operation::Enable, b"now\ndelayed").status,
        StatusCode::Ok
    );
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(
        request(&connection, 2, Operation::IsActive, b"delayed").status,
        StatusCode::Ok
    );
    assert_eq!(
        request(&connection, 3, Operation::Enable, b"now\nbroken").status,
        StatusCode::ServiceFailure
    );
    assert_eq!(
        request(&connection, 4, Operation::IsEnabled, b"broken").status,
        StatusCode::Ok
    );
}

#[test]
fn disable_now_waits_for_stop_helper_and_force_bypasses_it() {
    let mut fixture = Fixture::new();
    let marker = fixture.root.join("stop-helper");
    let script = format!("sleep 0.15; touch {}", marker.display());
    fixture.write_service(
        "slow",
        &["/bin/sleep", "30"],
        &format!("[actions]\nstop = [\"/bin/sh\", \"-c\", {script:?}]"),
    );
    let connection = fixture.boot(&[]);
    assert_eq!(
        request(&connection, 1, Operation::Enable, b"now\nslow").status,
        StatusCode::Ok
    );
    assert_eq!(
        request(&connection, 2, Operation::Disable, b"now\nslow").status,
        StatusCode::Ok
    );
    assert!(marker.is_file());
    fs::remove_file(&marker).unwrap();
    assert_eq!(
        request(&connection, 3, Operation::Start, b"slow").status,
        StatusCode::Ok
    );
    assert_eq!(
        request(&connection, 4, Operation::StopForce, b"slow").status,
        StatusCode::Ok
    );
    assert!(!marker.exists());
}

#[test]
fn leader_exit_reaps_descendants_before_restarting() {
    let mut fixture = Fixture::new();
    let child_file = fixture.root.join("child");
    let script = format!(
        "sleep 30 & echo $! > {}; sleep 0.1; exit 0",
        child_file.display()
    );
    fixture.write_service("tree", &["/bin/sh", "-c", &script], "");
    let connection = fixture.boot(&[]);
    assert_eq!(
        request(&connection, 1, Operation::Start, b"tree").status,
        StatusCode::Ok
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let pid = loop {
        if let Ok(source) = fs::read_to_string(&child_file)
            && let Ok(pid) = source.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "child did not publish its PID");
        thread::sleep(Duration::from_millis(5));
    };
    while PathBuf::from(format!("/proc/{pid}")).exists() {
        assert!(
            Instant::now() < deadline,
            "leader left descendant {pid} behind: {:?}",
            fs::read_to_string(format!("/proc/{pid}/status"))
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        request(&connection, 2, Operation::Restart, b"tree").status,
        StatusCode::Ok
    );
    assert_eq!(
        request(&connection, 3, Operation::Stop, b"tree").status,
        StatusCode::Ok
    );
}

#[test]
fn dry_run_matches_apply_without_mutation_and_metadata_does_not_restart() {
    let mut fixture = Fixture::new();
    fixture.write_service("old", &["/bin/sleep", "30"], "");
    let connection = fixture.boot(&["old"]);
    assert_eq!(
        request(&connection, 1, Operation::Start, b"old").status,
        StatusCode::Ok
    );
    let config = fixture.root.join("etc/loom/loom.toml");
    fixture.write_service("new", &["/bin/true"], "type = \"oneshot\"");
    fs::write(
        &config,
        "schema_version = 1\ndefault_group = \"boot\"\n[groups.boot]\nwants = [\"new\"]\n",
    )
    .unwrap();
    let original = fs::read(&config).unwrap();
    send(&connection, Operation::ApplyDryRun, b"toml\n");
    let preview = receive(&connection);
    assert_eq!(preview.status, StatusCode::Ok);
    let plan = document(&preview);
    assert_eq!(plan["plan"]["start"][0].as_str(), Some("new"));
    assert_eq!(plan["plan"]["stop"][0].as_str(), Some("old"));
    assert_eq!(fs::read(&config).unwrap(), original);
    assert_eq!(
        request(&connection, 2, Operation::IsActive, b"old").status,
        StatusCode::Ok
    );
    assert_eq!(
        request(&connection, 3, Operation::Apply, b"").status,
        StatusCode::Ok
    );
    assert_eq!(
        request(&connection, 4, Operation::IsActive, b"new").status,
        StatusCode::Ok
    );
    assert_eq!(
        request(&connection, 5, Operation::IsActive, b"old").status,
        StatusCode::ServiceFailure
    );
    let source = fixture.root.join("etc/loom/services/new.toml");
    let contents = fs::read_to_string(&source).unwrap().replacen(
        "schema_version = 1",
        "schema_version = 1\ndescription = \"Changed\"",
        1,
    );
    fs::write(&source, contents).unwrap();
    send(&connection, Operation::ApplyDryRun, b"toml\n");
    assert!(
        document(&receive(&connection))["plan"]["restart"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        request(&connection, 6, Operation::Apply, b"").status,
        StatusCode::Ok
    );
    send(&connection, Operation::Status, b"toml\nnew");
    assert_eq!(
        document(&receive(&connection))["services"][0]["generation"].as_integer(),
        Some(1)
    );
    fixture.write_service("bad-account", &["/bin/true"], "user = \"does-not-exist\"");
    assert_eq!(
        request(&connection, 7, Operation::ApplyDryRun, b"").status,
        StatusCode::InvalidRequest
    );
    assert_eq!(
        request(&connection, 8, Operation::Status, b"").status,
        StatusCode::Ok
    );
}

#[test]
fn structured_status_is_chunked_without_truncation() {
    let mut fixture = Fixture::new();
    for index in 0..600 {
        fixture.write_service(
            &format!("long-service-name-for-chunking-{index:04}"),
            &["/bin/true"],
            "",
        );
    }
    let connection = fixture.boot(&[]);
    send(&connection, Operation::List, b"toml\n");
    let report = receive(&connection);
    assert_eq!(report.status, StatusCode::Ok);
    assert!(report.payload.len() > loom::protocol::MAX_PAYLOAD_LEN);
    assert_eq!(document(&report)["services"].as_array().unwrap().len(), 603);
}

#[test]
fn installed_user_manager_autostarts_once_for_concurrent_clients() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let prefix = fixture.root.join("usr");
    fs::create_dir_all(prefix.join("bin")).unwrap();
    fs::create_dir_all(prefix.join("lib/loom")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_loom"), prefix.join("lib/loom/loom")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_loomctl"), prefix.join("bin/loomctl")).unwrap();
    fs::set_permissions(
        fixture.root.join("run/user"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            Command::new(prefix.join("bin/loomctl"))
                .args(["--user", "--format", "toml", "status", "--root"])
                .arg(&fixture.root)
                .env("XDG_RUNTIME_DIR", fixture.root.join("run/user"))
                .env("XDG_CONFIG_HOME", fixture.root.join("etc"))
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let mut pid = None;
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: toml_edit::DocumentMut =
            String::from_utf8(output.stdout).unwrap().parse().unwrap();
        let manager = report["manager_pid"].as_integer().unwrap();
        if let Some(previous) = pid {
            assert_eq!(manager, previous);
        }
        pid = Some(manager);
    }
    let pid = pid.unwrap().to_string();
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while fixture.root.join("run/user/loom/control.sock").exists() {
        assert!(Instant::now() < deadline, "user manager did not stop");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn dry_run_does_not_autostart_an_absent_user_manager() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let runtime = fixture.root.join("run/user");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = fixture.root.join("etc/loom/loom.toml");
    let before = fs::read(&config_path).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_loomctl"))
        .args(["--user", "apply", "--dry-run", "--root"])
        .arg(&fixture.root)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", fixture.root.join("etc"))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(4));
    assert!(!runtime.join("loom-start.lock").exists());
    assert!(!runtime.join("loom/control.sock").exists());
    assert_eq!(fs::read(config_path).unwrap(), before);
}
