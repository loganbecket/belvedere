//! The real service reading a made-up Thunderbird profile: startup scan,
//! a new message landing, a copy in another folder, and a restart.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{stop, wait_ready, Bus};

fn message(id: &str, subject: &str, date: &str) -> String {
    format!(
        "From - Mon Oct 20 09:00:00 2026\nX-Mozilla-Status: 0001\nFrom: Billing <billing@example.invalid>\nTo: me@example.invalid\nSubject: {subject}\nDate: {date}\nMessage-ID: <{id}@example.invalid>\n\nThis is {subject}.\n"
    )
}

/// A profile with one IMAP account: INBOX (two messages, one old) and an
/// Archive folder.
fn fake_profile(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
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
    let inbox = imap.join("INBOX");
    let archive = imap.join("Archive");
    let recent = chrono::Utc::now() - chrono::Duration::days(2);
    let old = chrono::Utc::now() - chrono::Duration::days(60);
    std::fs::write(
        &inbox,
        message("old", "Ancient newsletter", &old.to_rfc2822())
            + &message("recent", "Your statement", &recent.to_rfc2822()),
    )
    .unwrap();
    std::fs::write(imap.join("INBOX.msf"), "// msf").unwrap();
    std::fs::write(&archive, "").unwrap();
    std::fs::write(imap.join("Archive.msf"), "// msf").unwrap();
    (profile, inbox, archive)
}

async fn subjects(proxy: &belvedere_core::ipc::ServiceProxy<'_>) -> Vec<String> {
    proxy
        .list_recent_mail(50)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.subject)
        .collect()
}

async fn wait_for_subject(
    proxy: &belvedere_core::ipc::ServiceProxy<'_>,
    subject: &str,
    within: Duration,
) -> Duration {
    let started = Instant::now();
    loop {
        if subjects(proxy).await.iter().any(|s| s == subject) {
            return started.elapsed();
        }
        assert!(started.elapsed() < within, "{subject:?} never appeared");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn reads_new_mail_and_never_twice() {
    let dir = tempfile::tempdir().unwrap();
    let (profile, inbox, archive) = fake_profile(dir.path());
    let db_path = dir.path().join("belvedere.db");
    let bus = Bus::start().await;
    let spawn = || {
        let mut cmd = bus.service_command(&db_path);
        cmd.env("BELVEDERE_TB_PROFILE", &profile);
        cmd.spawn().unwrap()
    };

    let service = spawn();
    let conn = bus.connect().await;
    let proxy = wait_ready(&conn).await;

    // Startup scan: the recent message is in, the 60-day-old one is not.
    wait_for_subject(&proxy, "Your statement", Duration::from_secs(10)).await;
    let seen = subjects(&proxy).await;
    assert_eq!(seen, ["Your statement"]);
    let first = proxy.list_recent_mail(50).await.unwrap().remove(0);
    assert_eq!(first.from_addr, "billing@example.invalid");
    assert_eq!(first.folder, "INBOX");
    assert!(first.snippet.contains("This is Your statement."));

    // Thunderbird downloads a new message: appended to INBOX.
    let now = chrono::Utc::now();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&inbox)
        .unwrap();
    write!(
        f,
        "{}",
        message("fresh", "Your bill is due", &now.to_rfc2822())
    )
    .unwrap();
    drop(f);
    let took = wait_for_subject(&proxy, "Your bill is due", Duration::from_secs(60)).await;
    eprintln!("new message noticed after {took:?}");
    assert!(took < Duration::from_secs(60));

    // The same message copied to Archive is not new.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&archive)
        .unwrap();
    write!(
        f,
        "{}",
        message("fresh", "Your bill is due", &now.to_rfc2822())
    )
    .unwrap();
    drop(f);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let seen = subjects(&proxy).await;
    assert_eq!(seen.iter().filter(|s| *s == "Your bill is due").count(), 1);
    assert_eq!(seen.len(), 2);

    // Restart: nothing is processed again.
    stop(service).await;
    let service = spawn();
    let proxy = wait_ready(&conn).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let seen = subjects(&proxy).await;
    assert_eq!(seen.len(), 2);
    stop(service).await;
}
