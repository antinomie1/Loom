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
            "schema_version = 1\ndefault_group = \"boot\"\n[groups.boot]\nwants = [\"probe\"]\n",
        )
        .unwrap();
        fs::write(
            root.join("etc/loom/services/probe.toml"),
            "schema_version = 1\n[process]\ncommand = [\"/bin/true\"]\ntype = \"oneshot\"\n[io]\nstdout = \"null\"\nstderr = \"null\"\n",
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
        let request = Packet {
            kind: MessageKind::Request,
            request_id,
            operation: Operation::Status,
            status: StatusCode::Ok,
            more: false,
            payload: b"probe".to_vec(),
        };
        connection.send(&request.encode().unwrap()).unwrap();
        let response = Packet::decode(&connection.receive().unwrap().unwrap()).unwrap();
        assert_eq!(response.status, StatusCode::Ok);
        if String::from_utf8(response.payload)
            .unwrap()
            .contains("probe\tactive")
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("probe did not become active");
}
