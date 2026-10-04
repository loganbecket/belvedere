//! Scanning one mbox file for messages not yet seen, reading only what's
//! new when possible.

use std::fs::File;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};

use super::mbox::{self, Messages};
use super::parse::normalize;
use crate::db::{Db, MboxState, NewMailMessage};

/// How far back the first scan of a folder looks.
pub const FIRST_RUN_WINDOW: Duration = Duration::days(14);

/// How many leading bytes identify a file as "the same file as before".
const HEAD_LEN: usize = 256;

/// What one scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    /// Messages recorded for the first time.
    pub new: usize,
    /// Messages seen but already known (same Message-ID).
    pub known: usize,
    /// Messages skipped: too old on a first scan, or deleted in Thunderbird.
    pub skipped: usize,
    /// Whether the file had to be read from the start.
    pub full_rescan: bool,
}

/// Scans `mbox` for `account`/`folder`, recording new messages in `db`.
///
/// - First time this file is seen: read it all, but keep only messages
///   dated within the last 14 days (a message with no date is kept).
/// - Later: read from where the last scan stopped, if the file still
///   starts with the same bytes and has not shrunk; otherwise (Thunderbird
///   compacted or rewrote it) read it all again. Known Message-IDs are
///   skipped either way, so nothing is ever recorded twice.
pub fn scan_folder(
    db: &Db,
    account: &str,
    folder: &str,
    mbox: &Path,
    now: DateTime<Utc>,
) -> std::io::Result<ScanOutcome> {
    let mut outcome = ScanOutcome::default();
    let path_key = mbox.to_string_lossy().into_owned();
    let meta = std::fs::metadata(mbox)?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let head = read_head(mbox)?;
    let previous = db.mbox_state(&path_key).map_err(std::io::Error::other)?;
    let (start, first_scan) = match &previous {
        Some(p) if p.head == head && size >= p.scanned_to => (p.scanned_to as u64, false),
        Some(_) => {
            outcome.full_rescan = true;
            (0, false)
        }
        None => (0, true),
    };
    if !first_scan
        && !outcome.full_rescan
        && size == previous.as_ref().map(|p| p.scanned_to).unwrap_or(-1)
    {
        // Nothing new.
        return Ok(outcome);
    }

    let cutoff = now - FIRST_RUN_WINDOW;
    let file = File::open(mbox)?;
    // Mail files can be gigabytes. Tell the kernel not to keep what we
    // read in the page cache on our account, so a first scan doesn't
    // look like Belvedere is holding gigabytes of memory.
    let drop_cache = DropCache::open(mbox)?;
    let mut messages = Messages::from_offset(file, start)?;
    let mut scanned_to = start;
    for raw in messages.by_ref() {
        let raw = raw?;
        scanned_to = raw.offset + raw.bytes.len() as u64;
        let body = mbox::strip_separator(&raw.bytes);
        if mbox::mozilla_status(body).is_some_and(|s| s & mbox::STATUS_EXPUNGED != 0) {
            outcome.skipped += 1;
            continue;
        }
        let Some(n) = normalize(body) else {
            outcome.skipped += 1;
            continue;
        };
        if first_scan {
            if let Ok(d) = DateTime::parse_from_rfc3339(&n.date) {
                if d.with_timezone(&Utc) < cutoff {
                    outcome.skipped += 1;
                    continue;
                }
            }
        }
        let new = NewMailMessage {
            message_id: n.message_id,
            account: account.to_string(),
            folder: folder.to_string(),
            from_addr: n.from_addr,
            from_name: n.from_name,
            to_addrs: n.to_addrs,
            subject: n.subject,
            date: n.date,
            body_text: n.body_text,
            attachments: n.attachments,
            mbox_path: path_key.clone(),
            mbox_offset: raw.offset as i64,
        };
        match db.record_mail(&new).map_err(std::io::Error::other)? {
            (_, true) => outcome.new += 1,
            (_, false) => outcome.known += 1,
        }
    }

    drop(messages);
    drop_cache.now();
    db.set_mbox_state(&MboxState {
        mbox_path: path_key,
        scanned_to: scanned_to as i64,
        size,
        mtime,
        head,
    })
    .map_err(std::io::Error::other)?;
    Ok(outcome)
}

/// Asks the kernel to evict a file's pages from the cache once we're done
/// with them (`posix_fadvise(DONTNEED)`). Advisory and harmless if ignored.
struct DropCache {
    file: File,
}

impl DropCache {
    fn open(path: &Path) -> std::io::Result<Self> {
        Ok(DropCache {
            file: File::open(path)?,
        })
    }

    fn now(&self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::posix_fadvise(self.file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
}

fn read_head(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; HEAD_LEN];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn message(id: &str, date: &str, subject: &str) -> String {
        format!(
            "From - Mon Oct 20 09:00:00 2026\nFrom: a@example.invalid\nTo: me@example.invalid\nSubject: {subject}\nDate: {date}\nMessage-ID: <{id}@example.invalid>\n\nbody of {id}\n"
        )
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-20T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn first_scan_keeps_only_the_last_two_weeks_then_reads_only_new_bytes() {
        let db = Db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mbox = dir.path().join("INBOX");
        let mut f = File::create(&mbox).unwrap();
        write!(
            f,
            "{}",
            message("old", "Mon, 1 Sep 2026 09:00:00 +0000", "Old")
        )
        .unwrap();
        write!(
            f,
            "{}",
            message("recent", "Sun, 19 Oct 2026 09:00:00 +0000", "Recent")
        )
        .unwrap();
        write!(f, "{}", message("undated", "", "Undated")).unwrap();
        drop(f);

        let first = scan_folder(&db, "acct", "INBOX", &mbox, now()).unwrap();
        assert_eq!(
            first,
            ScanOutcome {
                new: 2,
                known: 0,
                skipped: 1,
                full_rescan: false
            }
        );
        assert_eq!(db.mail_count().unwrap(), 2);

        // Nothing changed: nothing read.
        let again = scan_folder(&db, "acct", "INBOX", &mbox, now()).unwrap();
        assert_eq!(again, ScanOutcome::default());

        // A new message appended: only it is read, and old dates no longer matter.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&mbox)
            .unwrap();
        write!(
            f,
            "{}",
            message(
                "later-old-date",
                "Mon, 1 Sep 2026 09:00:00 +0000",
                "Appended"
            )
        )
        .unwrap();
        drop(f);
        let third = scan_folder(&db, "acct", "INBOX", &mbox, now()).unwrap();
        assert_eq!(
            third,
            ScanOutcome {
                new: 1,
                known: 0,
                skipped: 0,
                full_rescan: false
            }
        );
        assert_eq!(db.mail_count().unwrap(), 3);
    }

    #[test]
    fn a_rewritten_file_is_rescanned_without_duplicates() {
        let db = Db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mbox = dir.path().join("INBOX");
        std::fs::write(
            &mbox,
            message("a", "Sun, 19 Oct 2026 09:00:00 +0000", "A")
                + &message("b", "Sun, 19 Oct 2026 10:00:00 +0000", "B"),
        )
        .unwrap();
        scan_folder(&db, "acct", "INBOX", &mbox, now()).unwrap();
        assert_eq!(db.mail_count().unwrap(), 2);

        // Thunderbird compacts: message a is gone, c is new, file shrank/changed.
        std::fs::write(
            &mbox,
            message("b", "Sun, 19 Oct 2026 10:00:00 +0000", "B")
                + &message("c", "Mon, 20 Oct 2026 10:00:00 +0000", "C"),
        )
        .unwrap();
        let out = scan_folder(&db, "acct", "INBOX", &mbox, now()).unwrap();
        assert!(out.full_rescan);
        assert_eq!((out.new, out.known), (1, 1));
        assert_eq!(db.mail_count().unwrap(), 3);
    }

    #[test]
    fn moved_messages_are_not_new_and_deleted_ones_are_skipped() {
        let db = Db::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("INBOX");
        let archive = dir.path().join("Archive");
        let m = message("same", "Sun, 19 Oct 2026 09:00:00 +0000", "Same");
        std::fs::write(&inbox, &m).unwrap();
        std::fs::write(&archive, &m).unwrap();
        assert_eq!(
            scan_folder(&db, "acct", "INBOX", &inbox, now())
                .unwrap()
                .new,
            1
        );
        let moved = scan_folder(&db, "acct", "Archive", &archive, now()).unwrap();
        assert_eq!((moved.new, moved.known), (0, 1));

        let deleted = m
            .replace(
                "From: a@example.invalid",
                "X-Mozilla-Status: 0009\nFrom: a@example.invalid",
            )
            .replace("<same@", "<gone@");
        let trash = dir.path().join("Trash");
        std::fs::write(&trash, deleted).unwrap();
        let out = scan_folder(&db, "acct", "Trash", &trash, now()).unwrap();
        assert_eq!(out.skipped, 1);
        assert_eq!(db.mail_count().unwrap(), 1);
    }

    #[test]
    fn survives_a_restart_without_reprocessing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("b.db");
        let mbox = dir.path().join("INBOX");
        std::fs::write(&mbox, message("a", "Sun, 19 Oct 2026 09:00:00 +0000", "A")).unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            assert_eq!(
                scan_folder(&db, "acct", "INBOX", &mbox, now()).unwrap().new,
                1
            );
        }
        let db = Db::open(&db_path).unwrap();
        let out = scan_folder(&db, "acct", "INBOX", &mbox, now()).unwrap();
        assert_eq!(out, ScanOutcome::default());
        assert_eq!(db.mail_count().unwrap(), 1);
    }
}
