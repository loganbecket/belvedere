//! Finding the user's files on demand: a walk of the home folder matched
//! against a few plain criteria. Read-only, nothing is opened; only
//! names, sizes, and dates are looked at. Credential locations are never
//! entered, whatever the query.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// Places that hold secrets. Never entered, never listed, even when
/// hidden folders are searched. Paths are relative to home; a bare name
/// matches a file or folder of that name anywhere.
pub const CREDENTIAL_LOCATIONS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".password-store",
    ".local/share/keyrings",
    ".mozilla",
    ".thunderbird",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/microsoft-edge",
    ".var/app/com.google.Chrome",
    ".var/app/com.brave.Browser",
    ".var/app/org.mozilla.firefox",
    ".var/app/org.mozilla.Thunderbird",
    ".var/app/org.mozilla.thunderbird_esr",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".docker",
    ".kube",
    ".netrc",
    ".pgpass",
    ".env",
    ".env.local",
    ".git-credentials",
    "id_rsa",
    "id_ed25519",
    "credentials.json",
    "secrets.json",
];

/// Folders full of program files, not the user's own; skipped by default.
pub const NOISE_FOLDERS: &[&str] = &[
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".git",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".gradle",
    ".m2",
    "Trash",
];

/// What to look for. Every field present must match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    /// Words that must all appear in the file name (case-insensitive).
    #[serde(default)]
    pub name: String,
    /// A kind: `pdf`, `image`, `document`, `spreadsheet`, `presentation`,
    /// `text`, `archive`, `audio`, `video`, `code`, or an extension.
    #[serde(default)]
    pub kind: String,
    /// `YYYY-MM-DD`, inclusive, local days.
    #[serde(default)]
    pub modified_after: Option<String>,
    #[serde(default)]
    pub modified_before: Option<String>,
    /// Bytes.
    #[serde(default)]
    pub min_size: Option<u64>,
    #[serde(default)]
    pub max_size: Option<u64>,
    /// Search hidden folders too (credential locations stay out).
    #[serde(default)]
    pub hidden: bool,
}

/// One file found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub path: String,
    pub size: u64,
    /// RFC 3339, local offset.
    pub modified: String,
}

/// The extensions a kind word covers.
pub fn extensions_for(kind: &str) -> Vec<&'static str> {
    match kind
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => vec!["pdf"],
        "image" | "images" | "photo" | "photos" | "picture" | "pictures" => {
            vec![
                "jpg", "jpeg", "png", "gif", "webp", "heic", "bmp", "tif", "tiff", "svg",
            ]
        }
        "document" | "documents" | "doc" | "word" => {
            vec!["doc", "docx", "odt", "rtf", "pages", "pdf"]
        }
        "spreadsheet" | "spreadsheets" | "excel" | "sheet" => {
            vec!["xls", "xlsx", "ods", "csv", "numbers"]
        }
        "presentation" | "presentations" | "slides" | "powerpoint" => {
            vec!["ppt", "pptx", "odp", "key"]
        }
        "text" | "note" | "notes" => vec!["txt", "md", "markdown", "rst", "org"],
        "archive" | "archives" | "zip" => vec!["zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar"],
        "audio" | "music" | "sound" => vec!["mp3", "flac", "wav", "ogg", "m4a", "aac"],
        "video" | "videos" | "movie" | "movies" => vec!["mp4", "mkv", "mov", "avi", "webm"],
        "code" | "source" => vec![
            "rs", "py", "js", "ts", "go", "c", "h", "cpp", "java", "rb", "sh", "toml", "json",
            "yaml", "yml",
        ],
        "" => vec![],
        other => vec![Box::leak(other.to_string().into_boxed_str())],
    }
}

fn day_of(modified: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(modified)
        .ok()
        .map(|d| d.with_timezone(&Local).date_naive())
}

/// Whether a file (by name, size, and modified time) matches the query.
/// Shared by the walker and the eval harness.
pub fn matches(query: &Query, path: &str, size: u64, modified: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    for word in query.name.to_lowercase().split_whitespace() {
        if !name.contains(word) {
            return false;
        }
    }
    let exts = extensions_for(&query.kind);
    if !exts.is_empty() {
        let ext = Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !exts.contains(&ext.as_str()) {
            return false;
        }
    }
    if let Some(after) = query
        .modified_after
        .as_deref()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
    {
        if day_of(modified).is_none_or(|d| d < after) {
            return false;
        }
    }
    if let Some(before) = query
        .modified_before
        .as_deref()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
    {
        if day_of(modified).is_none_or(|d| d > before) {
            return false;
        }
    }
    if query.min_size.is_some_and(|m| size < m) || query.max_size.is_some_and(|m| size > m) {
        return false;
    }
    true
}

/// Whether a path under `home` is a credential location (or inside one).
pub fn is_credential(home: &Path, path: &Path, extra: &[String]) -> bool {
    let rel = path.strip_prefix(home).unwrap_or(path);
    let rel_str = rel.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    CREDENTIAL_LOCATIONS
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().cloned())
        .any(|loc| {
            if loc.contains('/') || loc.starts_with('.') && loc.len() > 1 && !loc.contains('.') {
                rel_str == loc || rel_str.starts_with(&format!("{loc}/"))
            } else {
                // A bare name: anywhere.
                name == loc || rel_str == loc || rel_str.starts_with(&format!("{loc}/"))
            }
        })
}

/// What the walker chose to do, for logs and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkStats {
    pub files_seen: u64,
    pub folders_entered: u64,
    pub folders_skipped: u64,
    pub elapsed_ms: u128,
}

fn to_rfc3339(t: SystemTime) -> String {
    let d: DateTime<Utc> = t.into();
    d.with_timezone(&Local)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// Searches `home` with several threads. `extra_excludes` adds to the
/// credential list (the `file_search_excludes` setting). Returns the
/// newest `limit` matches.
pub fn search(
    home: &Path,
    query: &Query,
    extra_excludes: &[String],
    limit: usize,
) -> (Vec<Found>, WalkStats) {
    let started = std::time::Instant::now();
    let queue: Arc<Mutex<VecDeque<PathBuf>>> =
        Arc::new(Mutex::new(VecDeque::from([home.to_path_buf()])));
    let results: Arc<Mutex<Vec<Found>>> = Arc::new(Mutex::new(Vec::new()));
    let stats: Arc<Mutex<WalkStats>> = Arc::new(Mutex::new(WalkStats::default()));
    let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 16);
    let home_dev = std::fs::metadata(home).ok().map(dev_of);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let queue = queue.clone();
            let results = results.clone();
            let stats = stats.clone();
            let busy = busy.clone();
            let extra = extra_excludes.to_vec();
            scope.spawn(move || loop {
                let next = {
                    let mut q = queue.lock().unwrap();
                    match q.pop_front() {
                        Some(p) => {
                            busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Some(p)
                        }
                        None => None,
                    }
                };
                let Some(dir) = next else {
                    if busy.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(200));
                    continue;
                };
                let mut local_files = 0u64;
                let mut local_skipped = 0u64;
                let mut found = Vec::new();
                let mut more = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let Ok(kind) = entry.file_type() else {
                            continue;
                        };
                        if kind.is_symlink() {
                            continue;
                        }
                        if is_credential(home, &path, &extra) {
                            local_skipped += 1;
                            continue;
                        }
                        if kind.is_dir() {
                            let hidden = name.starts_with('.');
                            if NOISE_FOLDERS.contains(&name.as_str()) || (hidden && !query.hidden) {
                                local_skipped += 1;
                                continue;
                            }
                            // Stay on the home file system.
                            if let (Some(hd), Ok(meta)) = (home_dev, entry.metadata()) {
                                if dev_of(meta) != hd {
                                    local_skipped += 1;
                                    continue;
                                }
                            }
                            more.push(path);
                            continue;
                        }
                        if !kind.is_file() {
                            continue;
                        }
                        local_files += 1;
                        // Cheap checks first; metadata only when the name fits.
                        let quick = Query {
                            modified_after: None,
                            modified_before: None,
                            min_size: None,
                            max_size: None,
                            ..query.clone()
                        };
                        let p = path.to_string_lossy().into_owned();
                        if !matches(&quick, &p, 0, "") {
                            continue;
                        }
                        let Ok(meta) = entry.metadata() else { continue };
                        let modified = meta.modified().map(to_rfc3339).unwrap_or_default();
                        if matches(query, &p, meta.len(), &modified) {
                            found.push(Found {
                                path: p,
                                size: meta.len(),
                                modified,
                            });
                        }
                    }
                }
                {
                    let mut q = queue.lock().unwrap();
                    q.extend(more);
                }
                {
                    let mut s = stats.lock().unwrap();
                    s.files_seen += local_files;
                    s.folders_entered += 1;
                    s.folders_skipped += local_skipped;
                }
                if !found.is_empty() {
                    results.lock().unwrap().extend(found);
                }
                busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
    });
    let mut out = Arc::try_unwrap(results)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_default();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.path.cmp(&b.path)));
    out.truncate(limit);
    let mut st = stats.lock().unwrap().clone();
    st.elapsed_ms = started.elapsed().as_millis();
    (out, st)
}

#[cfg(unix)]
fn dev_of(meta: std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

/// A local day as an RFC 3339 range start, for callers turning "yesterday"
/// into a query.
pub fn local_day_string(day: NaiveDate) -> String {
    Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap_or_default())
        .earliest()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path();
        for d in [
            "Documents/Leases",
            "Downloads",
            "Pictures/2026",
            "Development/app/node_modules/x",
            ".ssh",
            ".config/google-chrome/Default",
            ".local/share/keyrings",
            ".hidden-notes",
            "Projects/site",
        ] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }
        let write = |p: &str, bytes: usize| std::fs::write(h.join(p), vec![b'x'; bytes]).unwrap();
        write("Documents/Leases/Oakridge lease 2026.pdf", 50_000);
        write("Documents/Leases/old lease.txt", 300);
        write("Downloads/statement-october.pdf", 20_000);
        write("Downloads/photo.jpg", 2_000_000);
        write("Pictures/2026/IMG_0001.HEIC", 3_000_000);
        write("Development/app/node_modules/x/lease.js", 10);
        write(".ssh/id_ed25519", 400);
        write(".config/google-chrome/Default/Login Data", 900);
        write(".local/share/keyrings/login.keyring", 500);
        write(".hidden-notes/lease notes.md", 100);
        write("Projects/site/.env", 60);
        write("Projects/site/index.html", 600);
        dir
    }

    fn names(found: &[Found]) -> Vec<String> {
        found
            .iter()
            .map(|f| {
                Path::new(&f.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn finds_by_name_kind_and_size_and_skips_noise_and_hidden_by_default() {
        let dir = tree();
        let q = Query {
            name: "lease".into(),
            ..Default::default()
        };
        let (found, stats) = search(dir.path(), &q, &[], 20);
        let mut got = names(&found);
        got.sort();
        assert_eq!(got, ["Oakridge lease 2026.pdf", "old lease.txt"]);
        assert!(stats.files_seen >= 6);
        assert!(stats.folders_skipped >= 5, "{stats:?}");
        // Hidden folders when asked; credential places still never.
        let (found, _) = search(
            dir.path(),
            &Query {
                hidden: true,
                ..q.clone()
            },
            &[],
            20,
        );
        let mut got = names(&found);
        got.sort();
        assert_eq!(
            got,
            ["Oakridge lease 2026.pdf", "lease notes.md", "old lease.txt"]
        );
        let (pdfs, _) = search(
            dir.path(),
            &Query {
                kind: "pdf".into(),
                ..Default::default()
            },
            &[],
            20,
        );
        let mut got = names(&pdfs);
        got.sort();
        assert_eq!(got, ["Oakridge lease 2026.pdf", "statement-october.pdf"]);
        let (big, _) = search(
            dir.path(),
            &Query {
                kind: "image".into(),
                min_size: Some(2_500_000),
                ..Default::default()
            },
            &[],
            20,
        );
        assert_eq!(names(&big), ["IMG_0001.HEIC"]);
        let (none, _) = search(
            dir.path(),
            &Query {
                name: "lease".into(),
                kind: "image".into(),
                ..Default::default()
            },
            &[],
            20,
        );
        assert!(none.is_empty());
    }

    #[test]
    fn credential_locations_are_never_entered_even_when_unreadable_or_asked_for() {
        let dir = tree();
        let h = dir.path();
        // Make the secret places unreadable: entering one would be an error
        // the walker would have to swallow; it must not even try.
        use std::os::unix::fs::PermissionsExt;
        for d in [".ssh", ".config/google-chrome", ".local/share/keyrings"] {
            std::fs::set_permissions(h.join(d), std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let everything = Query {
            hidden: true,
            ..Default::default()
        };
        let (found, stats) = search(h, &everything, &[], 100);
        let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
        for secret in ["id_ed25519", "Login Data", "login.keyring", ".env"] {
            assert!(
                !paths.iter().any(|p| p.ends_with(secret)),
                "{secret} listed: {paths:?}"
            );
        }
        assert!(stats.folders_skipped >= 4);
        // Asking by name does not help.
        let (found, _) = search(
            h,
            &Query {
                name: "id_ed25519".into(),
                hidden: true,
                ..Default::default()
            },
            &[],
            100,
        );
        assert!(found.is_empty());
        let (found, _) = search(
            h,
            &Query {
                name: ".env".into(),
                hidden: true,
                ..Default::default()
            },
            &[],
            100,
        );
        assert!(found.is_empty());
        // The setting adds places.
        let (found, _) = search(
            h,
            &Query {
                name: "index".into(),
                ..Default::default()
            },
            &["Projects/site".into()],
            100,
        );
        assert!(found.is_empty());
        for d in [".ssh", ".config/google-chrome", ".local/share/keyrings"] {
            std::fs::set_permissions(h.join(d), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(is_credential(h, &h.join(".aws/credentials"), &[]));
        assert!(is_credential(h, &h.join("work/.env"), &[]));
        assert!(!is_credential(h, &h.join("Documents/environment.pdf"), &[]));
    }

    #[test]
    fn dates_match_by_local_day() {
        let today = Local::now().date_naive();
        let q = Query {
            modified_after: Some(today.to_string()),
            ..Default::default()
        };
        let now = Local::now().to_rfc3339();
        assert!(matches(&q, "/x/a.pdf", 1, &now));
        let old = (Local::now() - chrono::Duration::days(3)).to_rfc3339();
        assert!(!matches(&q, "/x/a.pdf", 1, &old));
        let before = Query {
            modified_before: Some((today - chrono::Duration::days(1)).to_string()),
            ..Default::default()
        };
        assert!(matches(&before, "/x/a.pdf", 1, &old));
        assert!(!matches(&before, "/x/a.pdf", 1, &now));
        assert_eq!(extensions_for("Spreadsheet")[0], "xls");
        assert_eq!(extensions_for(".DOCX"), ["docx"]);
    }
}
