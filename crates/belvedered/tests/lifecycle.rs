//! Process-level checks: the service reports its version, stays alive,
//! and exits cleanly on SIGTERM. Runs on a private bus so it never touches
//! the real service.

mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use common::{stop, wait_ready, Bus};

#[test]
fn version_flag_prints_name_and_version_and_exits() {
    let out = Command::new(env!("CARGO_BIN_EXE_belvedered"))
        .arg("--version")
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("belvedered {}", belvedere_core::VERSION)
    );
}

#[tokio::test]
async fn stays_alive_then_exits_cleanly_on_sigterm() {
    let bus = Bus::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = bus.service_command(&dir.path().join("belvedere.db"));
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let conn = bus.connect().await;
    wait_ready(&conn).await;

    // Still running after a moment.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(child.try_wait().unwrap().is_none(), "service exited early");

    let stderr = child.stderr.take().unwrap();
    let status = stop(child).await;
    assert!(status.success(), "expected a clean exit, got {status}");

    // Without journald (as in CI) logs land on stderr; with it, stderr is quiet.
    let mut log = String::new();
    tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::BufReader::new(stderr), &mut log)
        .await
        .unwrap();
    if !log.is_empty() {
        assert!(log.contains("shutting down"), "stderr was: {log}");
    }
}
