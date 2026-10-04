//! Finding Thunderbird's data on this machine and reading it without ever
//! writing to it.
//!
//! Everything here opens files read-only. Databases are copied to a
//! scratch directory first and the copy is opened, so Thunderbird's own
//! files are never even opened for writing or locked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A Thunderbird profile directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub dir: PathBuf,
    /// Where it came from, e.g. "Flatpak (ESR)".
    pub source: String,
}

/// Where Thunderbird keeps profiles, in the order they're tried.
pub fn profile_roots() -> Vec<(PathBuf, &'static str)> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    vec![
        (
            home.join(".var/app/org.mozilla.thunderbird_esr/.thunderbird"),
            "Flatpak (ESR)",
        ),
        (
            home.join(".var/app/org.mozilla.Thunderbird/.thunderbird"),
            "Flatpak",
        ),
        (home.join(".thunderbird"), "native"),
        (home.join("snap/thunderbird/common/.thunderbird"), "Snap"),
    ]
}

/// The default profile: the first root whose `profiles.ini` names one
/// that actually exists and holds mail.
pub fn find_profile() -> Option<Profile> {
    profile_roots()
        .into_iter()
        .filter_map(|(root, source)| {
            let dir = default_profile_in(&root)?;
            Some(Profile {
                dir,
                source: source.to_string(),
            })
        })
        .find(|p| p.dir.join("prefs.js").is_file())
}

/// Reads `profiles.ini` under `root` and returns the default profile's
/// directory. Prefers the `[Install…]` section's `Default=`, then a
/// `[Profile…]` section with `Default=1`, then the only profile.
pub fn default_profile_in(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root.join("profiles.ini")).ok()?;
    let sections = parse_ini(&text);
    let resolve = |path: &str, relative: bool| -> PathBuf {
        if relative {
            root.join(path)
        } else {
            PathBuf::from(path)
        }
    };

    // [Install...] Default=<path> is the authoritative choice in modern
    // Thunderbird; its path is always profile-root relative.
    for (name, kv) in &sections {
        if name.starts_with("Install") {
            if let Some(p) = kv.get("Default") {
                let dir = root.join(p);
                if dir.is_dir() {
                    return Some(dir);
                }
            }
        }
    }
    let mut profiles: Vec<(PathBuf, bool)> = Vec::new();
    for (name, kv) in &sections {
        if name.starts_with("Profile") {
            if let Some(p) = kv.get("Path") {
                let relative = kv.get("IsRelative").map(|v| v == "1").unwrap_or(true);
                let is_default = kv.get("Default").map(|v| v == "1").unwrap_or(false);
                profiles.push((resolve(p, relative), is_default));
            }
        }
    }
    profiles
        .iter()
        .find(|(_, d)| *d)
        .or_else(|| (profiles.len() == 1).then(|| &profiles[0]))
        .map(|(p, _)| p.clone())
        .filter(|p| p.is_dir())
}

/// Minimal INI parser: section name -> key -> value.
fn parse_ini(text: &str) -> Vec<(String, HashMap<String, String>)> {
    let mut out: Vec<(String, HashMap<String, String>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push((name.to_string(), HashMap::new()));
        } else if let Some((k, v)) = line.split_once('=') {
            if let Some((_, kv)) = out.last_mut() {
                kv.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    out
}

/// A Thunderbird preference value.
#[derive(Debug, Clone, PartialEq)]
pub enum Pref {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Pref {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Pref::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Parses `prefs.js`: lines of `user_pref("key", value);`.
pub fn parse_prefs(text: &str) -> HashMap<String, Pref> {
    let mut prefs = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("user_pref(") else {
            continue;
        };
        let Some((key, rest)) = read_js_string(rest) else {
            continue;
        };
        let rest = rest
            .trim_start()
            .strip_prefix(',')
            .unwrap_or(rest)
            .trim_start();
        let value = if rest.starts_with('"') {
            read_js_string(rest).map(|(s, _)| Pref::Str(s))
        } else if let Some(v) = rest.strip_suffix(");") {
            let v = v.trim();
            match v {
                "true" => Some(Pref::Bool(true)),
                "false" => Some(Pref::Bool(false)),
                _ => v.parse::<i64>().ok().map(Pref::Int),
            }
        } else {
            None
        };
        if let Some(value) = value {
            prefs.insert(key, value);
        }
    }
    prefs
}

/// Reads a double-quoted JS string literal at the start of `s`, handling
/// `\"`, `\\`, and `\uXXXX`. Returns the string and the rest of `s`.
fn read_js_string(s: &str) -> Option<(String, &str)> {
    let s = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &s[i + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = (0..4)
                        .filter_map(|_| chars.next().map(|(_, c)| c))
                        .collect();
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// What a folder is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Junk,
    Trash,
    Archive,
    Templates,
    Outbox,
    Other,
}

impl FolderRole {
    pub fn as_str(self) -> &'static str {
        match self {
            FolderRole::Inbox => "inbox",
            FolderRole::Sent => "sent",
            FolderRole::Drafts => "drafts",
            FolderRole::Junk => "junk",
            FolderRole::Trash => "trash",
            FolderRole::Archive => "archive",
            FolderRole::Templates => "templates",
            FolderRole::Outbox => "outbox",
            FolderRole::Other => "other",
        }
    }

    /// From Thunderbird's folder flag bits (nsMsgFolderFlags).
    pub fn from_flags(flags: u64) -> Option<FolderRole> {
        const INBOX: u64 = 0x1000;
        const SENT: u64 = 0x200;
        const DRAFTS: u64 = 0x400;
        const JUNK: u64 = 0x4000_0000;
        const TRASH: u64 = 0x100;
        const ARCHIVE: u64 = 0x4000;
        const TEMPLATES: u64 = 0x40_0000;
        const QUEUE: u64 = 0x800;
        [
            (INBOX, FolderRole::Inbox),
            (TRASH, FolderRole::Trash),
            (JUNK, FolderRole::Junk),
            (SENT, FolderRole::Sent),
            (DRAFTS, FolderRole::Drafts),
            (ARCHIVE, FolderRole::Archive),
            (TEMPLATES, FolderRole::Templates),
            (QUEUE, FolderRole::Outbox),
        ]
        .into_iter()
        .find(|(bit, _)| flags & bit != 0)
        .map(|(_, role)| role)
    }

    /// From a folder's name, when the flags don't say.
    pub fn from_name(name: &str) -> FolderRole {
        let n = name.to_ascii_lowercase();
        let n = n.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-');
        if n == "inbox" {
            FolderRole::Inbox
        } else if n.starts_with("sent") {
            FolderRole::Sent
        } else if n.starts_with("draft") {
            FolderRole::Drafts
        } else if n.starts_with("junk") || n.starts_with("spam") || n.contains("bulk") {
            FolderRole::Junk
        } else if n.starts_with("trash") || n.starts_with("deleted") || n == "bin" {
            FolderRole::Trash
        } else if n.starts_with("archive") || n == "all mail" {
            FolderRole::Archive
        } else if n.starts_with("template") {
            FolderRole::Templates
        } else if n == "outbox" || n.starts_with("unsent") {
            FolderRole::Outbox
        } else {
            FolderRole::Other
        }
    }
}

/// One mail folder on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Path within the account, e.g. `INBOX` or `[Gmail]/Sent Mail`.
    pub path: String,
    pub role: FolderRole,
    /// The mbox file holding the messages, if any have been downloaded.
    pub mbox: Option<PathBuf>,
    /// The `.msf` index file.
    pub msf: PathBuf,
}

/// One mail account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Thunderbird's account key, e.g. `account12`.
    pub key: String,
    /// The display name Thunderbird shows (usually the address).
    pub name: String,
    /// `imap`, `pop3`, `none` (Local Folders), etc.
    pub kind: String,
    pub hostname: String,
    /// Where its folders live.
    pub dir: PathBuf,
    pub folders: Vec<Folder>,
}

/// Lists every account in `profile` with every folder, roles included.
pub fn accounts(profile: &Path) -> std::io::Result<Vec<Account>> {
    let prefs = parse_prefs(&std::fs::read_to_string(profile.join("prefs.js"))?);
    let flags = folder_flags(profile);
    let s = |k: &str| prefs.get(k).and_then(Pref::as_str).map(str::to_string);

    let ids = s("mail.accountmanager.accounts").unwrap_or_default();
    let mut out = Vec::new();
    for key in ids.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        let Some(server) = s(&format!("mail.account.{key}.server")) else {
            continue;
        };
        let kind = s(&format!("mail.server.{server}.type")).unwrap_or_default();
        let hostname = s(&format!("mail.server.{server}.hostname")).unwrap_or_default();
        let name = s(&format!("mail.server.{server}.name")).unwrap_or_else(|| hostname.clone());
        let dir = server_dir(
            profile,
            &kind,
            &hostname,
            s(&format!("mail.server.{server}.directory-rel")).as_deref(),
        );
        let Some(dir) = dir else {
            continue;
        };
        let mut folders = Vec::new();
        collect_folders(&dir, "", profile, &flags, &mut folders);
        folders.sort_by(|a, b| a.path.cmp(&b.path));
        out.push(Account {
            key: key.to_string(),
            name,
            kind,
            hostname,
            dir,
            folders,
        });
    }
    Ok(out)
}

/// The account's folder directory: from `directory-rel` (`[ProfD]…`) if
/// it exists, else the conventional `ImapMail/<host>` or `Mail/<host>`.
fn server_dir(
    profile: &Path,
    kind: &str,
    hostname: &str,
    directory_rel: Option<&str>,
) -> Option<PathBuf> {
    if let Some(rel) = directory_rel.and_then(|r| r.strip_prefix("[ProfD]")) {
        let d = profile.join(rel);
        if d.is_dir() {
            return Some(d);
        }
    }
    let candidates = match kind {
        "imap" => vec![profile.join("ImapMail").join(hostname)],
        "none" => vec![profile.join("Mail").join("Local Folders")],
        _ => vec![profile.join("Mail").join(hostname)],
    };
    candidates.into_iter().find(|d| d.is_dir())
}

/// Walks a folder directory: every `X.msf` is a folder, `X` beside it is
/// its mbox, `X.sbd/` holds its subfolders.
fn collect_folders(
    dir: &Path,
    prefix: &str,
    profile: &Path,
    flags: &HashMap<String, u64>,
    out: &mut Vec<Folder>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        let Some(name) = file_name.strip_suffix(".msf") else {
            continue;
        };
        let folder_path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let mbox = dir.join(name);
        let key = relative_key(profile, &path);
        let role = flags
            .get(&key)
            .and_then(|f| FolderRole::from_flags(*f))
            .unwrap_or_else(|| FolderRole::from_name(name));
        out.push(Folder {
            path: folder_path.clone(),
            role,
            mbox: mbox.is_file().then_some(mbox),
            msf: path.clone(),
        });
        let sub = dir.join(format!("{name}.sbd"));
        if sub.is_dir() {
            collect_folders(&sub, &folder_path, profile, flags, out);
        }
    }
}

/// A folder's identity across machines and sandboxes: its path relative
/// to the profile directory. Flatpak sees the profile at a different
/// absolute path than we do, so absolute paths can't be compared.
fn relative_key(profile: &Path, file: &Path) -> String {
    let profile_name = profile
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or_default();
    let s = file.to_string_lossy();
    match s.find(&format!("/{profile_name}/")) {
        Some(at) => s[at + profile_name.len() + 2..].to_string(),
        None => s.into_owned(),
    }
}

/// Folder flags from `folderCache.json`, keyed by profile-relative path.
fn folder_flags(profile: &Path) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(profile.join("folderCache.json")) else {
        return out;
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, serde_json::Value>>(&text) else {
        return out;
    };
    for (path, v) in map {
        if let Some(flags) = v.get("flags").and_then(|f| f.as_u64()) {
            out.insert(relative_key(profile, Path::new(&path)), flags);
        }
    }
    out
}

/// A read-only copy of one of Thunderbird's SQLite databases.
///
/// Thunderbird keeps its databases open and in WAL mode while it runs.
/// Copying the database file together with its `-wal` and `-shm` files
/// and opening the copy read-only gives a consistent view without
/// touching, locking, or writing anything of Thunderbird's.
pub struct Snapshot {
    pub conn: rusqlite::Connection,
    _dir: tempfile::TempDir,
}

impl Snapshot {
    pub fn open(db: &Path) -> std::io::Result<Snapshot> {
        let dir = tempfile::tempdir()?;
        let name = db
            .file_name()
            .ok_or_else(|| std::io::Error::other("database has no file name"))?;
        let copy = dir.path().join(name);
        std::fs::copy(db, &copy)?;
        for suffix in ["-wal", "-shm"] {
            let mut side = db.as_os_str().to_owned();
            side.push(suffix);
            let side = PathBuf::from(side);
            if side.is_file() {
                let mut target = copy.as_os_str().to_owned();
                target.push(suffix);
                std::fs::copy(&side, PathBuf::from(target))?;
            }
        }
        // The copy is ours; let SQLite fold the WAL in, then open read-only.
        {
            let c = rusqlite::Connection::open(&copy).map_err(std::io::Error::other)?;
            let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        }
        let conn = rusqlite::Connection::open_with_flags(
            &copy,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(std::io::Error::other)?;
        Ok(Snapshot { conn, _dir: dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_parse_strings_ints_bools_and_escapes() {
        let text = r#"
// Mozilla User Preferences
user_pref("mail.accountmanager.accounts", "account1,account2");
user_pref("mail.server.server1.type", "imap");
user_pref("mail.server.server1.name", "someone@example.invalid");
user_pref("mail.identity.id1.fullName", "Some \"Quoted\" Name é");
user_pref("mail.server.server1.port", 993);
user_pref("mail.server.server1.useSecAuth", false);
user_pref("broken line
"#;
        let p = parse_prefs(text);
        assert_eq!(
            p["mail.accountmanager.accounts"],
            Pref::Str("account1,account2".into())
        );
        assert_eq!(p["mail.server.server1.port"], Pref::Int(993));
        assert_eq!(p["mail.server.server1.useSecAuth"], Pref::Bool(false));
        assert_eq!(
            p["mail.identity.id1.fullName"],
            Pref::Str("Some \"Quoted\" Name é".into())
        );
        assert_eq!(p.len(), 6);
    }

    #[test]
    fn roles_from_flags_and_names() {
        assert_eq!(
            FolderRole::from_flags(0x1000 | 0x4),
            Some(FolderRole::Inbox)
        );
        assert_eq!(FolderRole::from_flags(0x1008_2114), Some(FolderRole::Trash));
        assert_eq!(
            FolderRole::from_flags(0x808_4014),
            Some(FolderRole::Archive)
        );
        assert_eq!(FolderRole::from_flags(0x808_0414), Some(FolderRole::Drafts));
        assert_eq!(FolderRole::from_flags(0x4000_0004), Some(FolderRole::Junk));
        assert_eq!(FolderRole::from_flags(0x4), None);

        assert_eq!(FolderRole::from_name("INBOX"), FolderRole::Inbox);
        assert_eq!(FolderRole::from_name("Sent Items-1"), FolderRole::Sent);
        assert_eq!(FolderRole::from_name("Deleted Items"), FolderRole::Trash);
        assert_eq!(FolderRole::from_name("Junk Email"), FolderRole::Junk);
        assert_eq!(FolderRole::from_name("Archives"), FolderRole::Archive);
        assert_eq!(FolderRole::from_name("All Mail"), FolderRole::Archive);
        assert_eq!(FolderRole::from_name("Unsent Messages"), FolderRole::Outbox);
        assert_eq!(FolderRole::from_name("Good Articles"), FolderRole::Other);
    }

    #[test]
    fn profiles_ini_install_section_wins_then_default_flag() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("abc.default")).unwrap();
        std::fs::create_dir_all(root.path().join("xyz.other")).unwrap();
        std::fs::write(
            root.path().join("profiles.ini"),
            "[Profile1]\nName=other\nIsRelative=1\nPath=xyz.other\nDefault=1\n\n[Profile0]\nName=default\nIsRelative=1\nPath=abc.default\n\n[InstallABC]\nDefault=abc.default\nLocked=1\n",
        )
        .unwrap();
        assert_eq!(
            default_profile_in(root.path()),
            Some(root.path().join("abc.default"))
        );

        std::fs::write(
            root.path().join("profiles.ini"),
            "[Profile1]\nPath=xyz.other\nDefault=1\n[Profile0]\nPath=abc.default\n",
        )
        .unwrap();
        assert_eq!(
            default_profile_in(root.path()),
            Some(root.path().join("xyz.other"))
        );

        std::fs::write(
            root.path().join("profiles.ini"),
            "[Profile0]\nPath=abc.default\n",
        )
        .unwrap();
        assert_eq!(
            default_profile_in(root.path()),
            Some(root.path().join("abc.default"))
        );
    }

    /// A made-up profile with two accounts and a few folders.
    pub fn fake_profile(root: &Path) -> PathBuf {
        let profile = root.join("tb").join("q1w2e3r4.default");
        let imap = profile.join("ImapMail").join("imap.example.invalid");
        let gmail_sbd = imap.join("[Mail].sbd");
        let local = profile.join("Mail").join("Local Folders");
        std::fs::create_dir_all(&gmail_sbd).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        for (p, body) in [
            (
                imap.join("INBOX"),
                "From - Mon Oct 20 09:00:00 2026\nSubject: hi\n\nbody\n",
            ),
            (imap.join("INBOX.msf"), "// msf"),
            (imap.join("[Mail].msf"), "// msf"),
            (gmail_sbd.join("Sent Mail.msf"), "// msf"),
            (gmail_sbd.join("Spam.msf"), "// msf"),
            (gmail_sbd.join("Receipts"), ""),
            (gmail_sbd.join("Receipts.msf"), "// msf"),
            (local.join("Trash"), ""),
            (local.join("Trash.msf"), "// msf"),
            (local.join("Unsent Messages.msf"), "// msf"),
        ] {
            std::fs::write(p, body).unwrap();
        }
        std::fs::write(
            root.join("tb").join("profiles.ini"),
            "[Profile0]\nName=default\nIsRelative=1\nPath=q1w2e3r4.default\nDefault=1\n",
        )
        .unwrap();
        std::fs::write(
            profile.join("prefs.js"),
            r#"user_pref("mail.accountmanager.accounts", "account1,account2");
user_pref("mail.account.account1.server", "server1");
user_pref("mail.account.account2.server", "server2");
user_pref("mail.server.server1.type", "imap");
user_pref("mail.server.server1.hostname", "imap.example.invalid");
user_pref("mail.server.server1.name", "someone@example.invalid");
user_pref("mail.server.server1.directory-rel", "[ProfD]ImapMail/imap.example.invalid");
user_pref("mail.server.server2.type", "none");
user_pref("mail.server.server2.hostname", "Local Folders");
user_pref("mail.server.server2.name", "Local Folders");
user_pref("mail.server.server2.directory-rel", "[ProfD]Mail/Local Folders");
"#,
        )
        .unwrap();
        // Flags as Thunderbird writes them, keyed by the path the sandboxed
        // Thunderbird sees (different prefix, same profile directory name).
        let cache = serde_json::json!({
            "/sandbox/home/.thunderbird/q1w2e3r4.default/ImapMail/imap.example.invalid/INBOX.msf": {"flags": 0x1004u64},
            "/sandbox/home/.thunderbird/q1w2e3r4.default/ImapMail/imap.example.invalid/[Mail].sbd/Sent Mail.msf": {"flags": 0x204u64},
            "/sandbox/home/.thunderbird/q1w2e3r4.default/ImapMail/imap.example.invalid/[Mail].sbd/Spam.msf": {"flags": 0x4000_0004u64},
            "/sandbox/home/.thunderbird/q1w2e3r4.default/Mail/Local Folders/Trash.msf": {"flags": 0x104u64},
        });
        std::fs::write(profile.join("folderCache.json"), cache.to_string()).unwrap();
        profile
    }

    #[test]
    fn lists_accounts_and_folders_with_roles() {
        let root = tempfile::tempdir().unwrap();
        let profile = fake_profile(root.path());
        assert_eq!(
            default_profile_in(&root.path().join("tb")),
            Some(profile.clone())
        );

        let accounts = accounts(&profile).unwrap();
        assert_eq!(accounts.len(), 2);
        let imap = &accounts[0];
        assert_eq!(
            (imap.key.as_str(), imap.kind.as_str()),
            ("account1", "imap")
        );
        assert_eq!(imap.name, "someone@example.invalid");
        let paths: Vec<(&str, FolderRole)> = imap
            .folders
            .iter()
            .map(|f| (f.path.as_str(), f.role))
            .collect();
        assert_eq!(
            paths,
            [
                ("INBOX", FolderRole::Inbox),
                ("[Mail]", FolderRole::Other),
                ("[Mail]/Receipts", FolderRole::Other),
                ("[Mail]/Sent Mail", FolderRole::Sent),
                ("[Mail]/Spam", FolderRole::Junk),
            ]
        );
        assert!(imap.folders[0].mbox.is_some(), "INBOX has downloaded mail");
        assert!(imap.folders[1].mbox.is_none(), "[Mail] is only a container");

        let local = &accounts[1];
        assert_eq!(local.kind, "none");
        let roles: Vec<_> = local.folders.iter().map(|f| f.role).collect();
        assert_eq!(roles, [FolderRole::Trash, FolderRole::Outbox]);
    }

    #[test]
    fn snapshot_reads_a_live_database_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("live.sqlite");
        let live = rusqlite::Connection::open(&db_path).unwrap();
        live.pragma_update(None, "journal_mode", "WAL").unwrap();
        live.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1), (2), (3);")
            .unwrap();
        // Keep `live` open, as Thunderbird would, with the rows still in the WAL.
        let before: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect();

        let snap = Snapshot::open(&db_path).unwrap();
        let n: i64 = snap
            .conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        assert!(
            snap.conn.execute_batch("INSERT INTO t VALUES (4)").is_err(),
            "the snapshot is read-only"
        );

        let after: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            before, after,
            "Thunderbird's files are byte-for-byte unchanged"
        );
        drop(live);
    }
}
