//! Shared helpers for tests that run the real service binary. Every test
//! gets a private D-Bus session and a private database, so nothing a
//! test does can reach the real service or the real data.

#![allow(dead_code)]

use std::path::PathBuf;
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
            // No morning briefing unless a test asks for it.
            .env("BELVEDERE_NO_BRIEFING", "1")
            // Never read the real mailbox from a test. Tests that want
            // mail point this at a made-up profile.
            .env(
                "BELVEDERE_TB_PROFILE",
                "/nonexistent/belvedere-test-profile",
            )
            .stdout(Stdio::null())
            // Not piped: nobody reads it, and a full pipe would stall the
            // service. `BELVEDERE_TEST_LOG=1` shows it on the terminal instead.
            .stderr(if std::env::var_os("BELVEDERE_TEST_LOG").is_some() {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
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

/// A made-up Thunderbird profile with one IMAP account and the given
/// folders, each starting empty. Returns the profile directory and each
/// folder's mbox file, in the order given.
pub fn fake_profile(root: &std::path::Path, folders: &[&str]) -> (PathBuf, Vec<PathBuf>) {
    let profile = root.join("abcd1234.default");
    let imap = profile.join("ImapMail").join("imap.example.invalid");
    std::fs::create_dir_all(&imap).unwrap();
    std::fs::write(
        profile.join("prefs.js"),
        r#"user_pref("mail.accountmanager.accounts", "account1");
user_pref("mail.account.account1.server", "server1");
user_pref("mail.server.server1.type", "imap");
user_pref("mail.server.server1.hostname", "imap.example.invalid");
user_pref("mail.server.server1.name", "someone@example.invalid");
user_pref("mail.server.server1.directory-rel", "[ProfD]ImapMail/imap.example.invalid");
"#,
    )
    .unwrap();
    let mut files = Vec::new();
    for name in folders {
        let f = imap.join(name);
        std::fs::write(&f, "").unwrap();
        std::fs::write(imap.join(format!("{name}.msf")), "// msf").unwrap();
        files.push(f);
    }
    (profile, files)
}

/// One message in Thunderbird's mbox form. `replies_to` becomes an
/// In-Reply-To header when given.
pub fn mbox_message(
    id: &str,
    from: &str,
    subject: &str,
    body: &str,
    date: &str,
    replies_to: Option<&str>,
) -> String {
    let reply = replies_to
        .map(|r| format!("In-Reply-To: <{r}@example.invalid>\n"))
        .unwrap_or_default();
    format!(
        "From - Mon Oct 20 09:00:00 2026\nX-Mozilla-Status: 0001\nFrom: {from}\nTo: me@example.invalid\nSubject: {subject}\nDate: {date}\nMessage-ID: <{id}@example.invalid>\n{reply}\n{body}\n"
    )
}

/// Appends text to a file.
pub fn append(path: &std::path::Path, text: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    write!(f, "{text}").unwrap();
}

/// User plus system CPU time the process has used so far.
pub fn cpu_time(pid: u32) -> Duration {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // Fields after the parenthesized command name; utime and stime are
    // the 14th and 15th fields overall.
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    let hz = unsafe { sysconf(2) } as u64; // _SC_CLK_TCK
    Duration::from_millis(ticks * 1000 / hz.max(1))
}

/// What a notification showed.
#[derive(Debug, Clone)]
pub struct Shown {
    pub summary: String,
    pub body: String,
    pub at: std::time::Instant,
}

/// A stand-in for the desktop's notification daemon that records what
/// it is asked to show.
pub struct FakeDaemon {
    pub shown: std::sync::Arc<std::sync::Mutex<Vec<Shown>>>,
    next_id: std::sync::Mutex<u32>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl FakeDaemon {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: &str,
        _replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<&str>,
        _hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        _expire_timeout: i32,
    ) -> u32 {
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        self.shown.lock().unwrap().push(Shown {
            summary: summary.to_string(),
            body: body.to_string(),
            at: std::time::Instant::now(),
        });
        *next
    }

    fn close_notification(&self, _id: u32) {}

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}

/// Serves a fake notification daemon on the bus; returns the record of
/// what it showed (keep the connection alive).
pub async fn fake_notifications(
    bus: &Bus,
) -> (
    zbus::Connection,
    std::sync::Arc<std::sync::Mutex<Vec<Shown>>>,
) {
    let shown = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at(
            "/org/freedesktop/Notifications",
            FakeDaemon {
                shown: shown.clone(),
                next_id: std::sync::Mutex::new(0),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    (conn, shown)
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
    fn sysconf(name: i32) -> i64;
}
