//! Shared helpers for tests that run the real service binary. Every test
//! gets a private D-Bus session and a private database, so nothing a
//! test does can reach the real service or the real data.

#![allow(dead_code)]

use std::process::Stdio;
use std::time::Duration;

use belvedere_core::ipc::ServiceProxy;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// A throwaway session bus. Dies with the struct.
pub struct Bus {
    daemon: Child,
    pub address: String,
}

impl Bus {
    pub async fn start() -> Self {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("dbus-daemon must be installed");
        let stdout = daemon.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let address = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("dbus-daemon printed no address in time")
            .unwrap()
            .expect("dbus-daemon closed stdout");
        Bus { daemon, address }
    }

    pub async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    /// The service binary, pointed at this bus and the given database,
    /// ready to spawn (so tests can add environment first).
    pub fn service_command(&self, db_path: &std::path::Path) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_belvedered"));
        cmd.env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .env("BELVEDERE_DB", db_path)
            .env("RUST_LOG", "info")
            .env("BELVEDERE_NO_WINDOW_LAUNCH", "1")
            // Never read the real mailbox from a test. Tests that want
            // mail point this at a made-up profile.
            .env(
                "BELVEDERE_TB_PROFILE",
                "/nonexistent/belvedere-test-profile",
            )
            .stdout(Stdio::null())
            // Not piped: nobody reads it, and a full pipe would stall the
            // service. Tests that want the log set `.stderr(piped())`.
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd
    }

    /// The service binary, pointed at this bus and the given database.
    pub fn spawn_service(&self, db_path: &std::path::Path) -> Child {
        self.service_command(db_path).spawn().unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.start_kill();
    }
}

/// Waits until the service answers Ping.
pub async fn wait_ready(conn: &zbus::Connection) -> ServiceProxy<'_> {
    let proxy = ServiceProxy::new(conn).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(reply) = proxy.ping().await {
            assert_eq!(reply, "pong");
            return proxy;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "service never answered Ping"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Sends SIGTERM and waits for a clean exit.
pub async fn stop(mut child: Child) -> std::process::ExitStatus {
    let pid = child.id().unwrap() as i32;
    unsafe { kill(pid, 15) };
    timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("service did not exit after SIGTERM")
        .unwrap()
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
