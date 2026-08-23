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
