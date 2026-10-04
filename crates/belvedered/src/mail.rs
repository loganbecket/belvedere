//! Keeps up with Thunderbird's mail: scans every folder at startup,
//! whenever a folder file changes, and on a sweep every few minutes that
//! catches anything the file watcher missed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use belvedere_core::db::Db;
use belvedere_core::mail::scan_folder;
use belvedere_core::thunderbird::{self, Account, Profile};
use chrono::Utc;
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::dbus::SharedDb;

/// Changes to one file are batched for this long before scanning, since
/// Thunderbird writes a message in several pieces.
const SETTLE: Duration = Duration::from_secs(2);

/// Minutes between sweeps when the setting is unset.
pub const DEFAULT_SWEEP_MINUTES: u64 = 5;

/// Time between sweeps: `BELVEDERE_SWEEP_SECS` (tests), else the
/// `sweep_minutes` setting, else five minutes. Never under a second.
pub fn sweep_interval(db: &SharedDb) -> Duration {
    if let Some(secs) = std::env::var("BELVEDERE_SWEEP_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Duration::from_secs(secs.max(1));
    }
    let minutes = lock(db)
        .get_setting("sweep_minutes")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|m| *m > 0)
        .unwrap_or(DEFAULT_SWEEP_MINUTES);
    Duration::from_secs(minutes * 60)
}

/// Whether to react to folder files changing. `BELVEDERE_NO_MAIL_WATCH`
/// turns it off (tests prove the sweep alone is enough).
fn watching_enabled() -> bool {
    std::env::var_os("BELVEDERE_NO_MAIL_WATCH").is_none()
}

fn lock(db: &SharedDb) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|e| e.into_inner())
}

/// The profile to read: `BELVEDERE_TB_PROFILE` (tests) or the default.
pub fn profile() -> Option<Profile> {
    if let Some(p) = std::env::var_os("BELVEDERE_TB_PROFILE") {
        return Some(Profile {
            dir: PathBuf::from(p),
            source: "override".into(),
        });
    }
    thunderbird::find_profile()
}

/// Scans every folder of every account once. Returns how many messages
/// were new.
pub fn scan_all(db: &SharedDb, accounts: &[Account]) -> usize {
    let mut total_new = 0;
    for account in accounts {
        for folder in &account.folders {
            let Some(mbox) = &folder.mbox else { continue };
            let result = scan_folder(&lock(db), &account.name, &folder.path, mbox, Utc::now());
            match result {
                Ok(out) => {
                    total_new += out.new;
                    if out.new > 0 || out.full_rescan {
                        info!(
                            account = account.name,
                            folder = folder.path,
                            new = out.new,
                            known = out.known,
                            skipped = out.skipped,
                            rescan = out.full_rescan,
                            "folder scanned"
                        );
                    }
                }
                Err(err) => warn!(
                    account = account.name,
                    folder = folder.path,
                    "could not scan folder: {err}"
                ),
            }
        }
    }
    total_new
}

/// Nice 10 and the idle I/O class for the calling thread.
fn lower_priority() {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE << 13,
        );
    }
}

/// Which account/folder an mbox path belongs to.
fn locate<'a>(
    accounts: &'a [Account],
    path: &Path,
) -> Option<(&'a Account, &'a thunderbird::Folder)> {
    for a in accounts {
        for f in &a.folders {
            if f.mbox.as_deref() == Some(path) {
                return Some((a, f));
            }
        }
    }
    None
}

/// Runs forever: the startup scan, then a scan of each folder file that
/// changes. `changed` is told the number of new messages after each scan
/// that found any, so other parts of the service can react.
pub async fn run(db: SharedDb, changed: mpsc::UnboundedSender<usize>) {
    let Some(profile) = profile() else {
        warn!("no Thunderbird profile found; mail reading is off");
        return;
    };
    let mut accounts = match thunderbird::accounts(&profile.dir) {
        Ok(a) => a,
        Err(err) => {
            warn!("could not read Thunderbird accounts: {err}");
            return;
        }
    };
    info!(profile = %profile.dir.display(), accounts = accounts.len(), folders = accounts.iter().map(|a| a.folders.len()).sum::<usize>(), "reading mail");

    let started = std::time::Instant::now();
    let db2 = db.clone();
    let accounts2 = accounts.clone();
    // The first scan can read gigabytes; it runs on its own low-priority
    // thread so the desktop never feels it.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("belvedere-mail-scan".into())
        .spawn(move || {
            lower_priority();
            let _ = done_tx.send(scan_all(&db2, &accounts2));
        })
        .expect("spawn mail scan thread");
    let new = done_rx.await.unwrap_or(0);
    info!(
        new,
        seconds = started.elapsed().as_secs_f32(),
        "startup mail scan complete"
    );
    if new > 0 {
        let _ = changed.send(new);
    }

    // Watch the mail directories for changes (unless turned off).
    let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
    let _watcher = if watching_enabled() {
        start_watcher(&profile.dir, tx)
    } else {
        info!("mail folder watching is off; relying on the sweep");
        None
    };

    let mut pending: HashSet<PathBuf> = HashSet::new();
    let sweep = tokio::time::sleep(sweep_interval(&db));
    tokio::pin!(sweep);
    loop {
        tokio::select! {
            Some(first) = rx.recv() => {
                // Collect changes until things settle, then scan each touched file.
                pending.insert(first);
                let settle = tokio::time::sleep(SETTLE);
                tokio::pin!(settle);
                loop {
                    tokio::select! {
                        Some(p) = rx.recv() => { pending.insert(p); }
                        _ = &mut settle => break,
                    }
                }
                let new = scan_touched(&db, &profile, &mut accounts, &mut pending).await;
                if new > 0 {
                    let _ = changed.send(new);
                }
            }
            _ = &mut sweep => {
                let new = sweep_once(&db, &profile, &mut accounts).await;
                if new > 0 {
                    let _ = changed.send(new);
                }
                sweep.as_mut().reset(tokio::time::Instant::now() + sweep_interval(&db));
            }
        }
    }
}

/// Starts watching the profile's mail directories; `None` if that fails
/// (the sweep still runs).
fn start_watcher(
    profile_dir: &Path,
    tx: mpsc::UnboundedSender<PathBuf>,
) -> Option<notify::RecommendedWatcher> {
    let mut watcher =
        match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                if matches!(
                    event.kind,
                    notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                ) {
                    for path in event.paths {
                        let _ = tx.send(path);
                    }
                }
            }
        }) {
            Ok(w) => w,
            Err(err) => {
                warn!("could not watch mail folders: {err}; relying on the sweep");
                return None;
            }
        };
    for dir in [profile_dir.join("ImapMail"), profile_dir.join("Mail")] {
        if dir.is_dir() {
            if let Err(err) = watcher.watch(&dir, RecursiveMode::Recursive) {
                warn!(dir = %dir.display(), "could not watch: {err}");
            }
        }
    }
    Some(watcher)
}

/// Scans the folder files the watcher saw change. Returns how many
/// messages were new.
async fn scan_touched(
    db: &SharedDb,
    profile: &Profile,
    accounts: &mut Vec<Account>,
    pending: &mut HashSet<PathBuf>,
) -> usize {
    // New folders may have appeared; refresh the account map.
    if pending
        .iter()
        .any(|p| locate(accounts, p).is_none() && p.extension().is_none())
    {
        if let Ok(a) = thunderbird::accounts(&profile.dir) {
            *accounts = a;
        }
    }
    let mut total_new = 0;
    for path in pending.drain() {
        let Some((account, folder)) = locate(accounts, &path) else {
            continue; // an .msf index, a filter log, something else
        };
        let Some(mbox) = &folder.mbox else { continue };
        let db2 = db.clone();
        let (name, fpath, mbox) = (account.name.clone(), folder.path.clone(), mbox.clone());
        let result = tokio::task::spawn_blocking(move || {
            scan_folder(&lock(&db2), &name, &fpath, &mbox, Utc::now())
        })
        .await;
        match result {
            Ok(Ok(out)) => {
                if out.new > 0 {
                    info!(
                        account = account.name,
                        folder = folder.path,
                        new = out.new,
                        "new mail"
                    );
                    total_new += out.new;
                }
            }
            Ok(Err(err)) => warn!(folder = folder.path, "scan failed: {err}"),
            Err(err) => warn!("scan task failed: {err}"),
        }
    }
    total_new
}

/// One sweep: re-read the account list (folders come and go) and check
/// every folder file for new bytes. A folder that has not changed costs
/// one metadata read; the model is never involved here. Returns how many
/// messages were new.
async fn sweep_once(db: &SharedDb, profile: &Profile, accounts: &mut Vec<Account>) -> usize {
    let started = std::time::Instant::now();
    match thunderbird::accounts(&profile.dir) {
        Ok(a) => *accounts = a,
        Err(err) => warn!("sweep could not re-read accounts: {err}"),
    }
    let db2 = db.clone();
    let accounts2 = accounts.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("belvedere-mail-sweep".into())
        .spawn(move || {
            lower_priority();
            let _ = done_tx.send(scan_all(&db2, &accounts2));
        });
    if let Err(err) = spawned {
        warn!("could not start the sweep: {err}");
        return 0;
    }
    let new = done_rx.await.unwrap_or(0);
    info!(
        new,
        folders = accounts.iter().map(|a| a.folders.len()).sum::<usize>(),
        millis = started.elapsed().as_millis() as u64,
        "sweep complete"
    );
    new
}
