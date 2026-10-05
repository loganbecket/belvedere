//! Hugging Face, as far as Belvedere needs it: searching for GGUF model
//! repositories, listing their files with sizes and checksums, and the
//! addresses to download from. Only the pure parts live here; the
//! service does the network calls, and only when the user asks.

use serde::{Deserialize, Serialize};

/// Where Hugging Face lives. Tests point `BELVEDERE_HF_BASE` at a local
/// stand-in.
pub fn base_url() -> String {
    std::env::var("BELVEDERE_HF_BASE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://huggingface.co".to_string())
        .trim_end_matches('/')
        .to_string()
}

pub fn search_url(query: &str, limit: usize) -> String {
    format!(
        "{}/api/models?search={}&filter=gguf&sort=downloads&direction=-1&limit={limit}",
        base_url(),
        percent_encode(query)
    )
}

pub fn files_url(repo: &str) -> String {
    format!("{}/api/models/{repo}?blobs=true", base_url())
}

pub fn download_url(repo: &str, file: &str) -> String {
    format!(
        "{}/{repo}/resolve/main/{}",
        base_url(),
        percent_encode_path(file)
    )
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_encode_path(s: &str) -> String {
    s.split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

/// A repository from a search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub downloads: u64,
    pub likes: u64,
}

/// A GGUF file in a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFile {
    pub name: String,
    pub size: u64,
    /// Lowercase hex, when the repository gives one (LFS files do).
    pub sha256: String,
    /// `fits`, `tight`, or `too big` for this machine's memory.
    pub fit: String,
    /// From the file name, e.g. `Q4_K_M`.
    pub quantization: String,
}

/// Reads a search response (a JSON array of repositories).
pub fn parse_search(json: &str) -> Result<Vec<Repo>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("bad search response: {e}"))?;
    let arr = v.as_array().ok_or("search response is not a list")?;
    Ok(arr
        .iter()
        .filter_map(|r| {
            let id = r
                .get("id")
                .or_else(|| r.get("modelId"))?
                .as_str()?
                .to_string();
            Some(Repo {
                id,
                downloads: r.get("downloads").and_then(|d| d.as_u64()).unwrap_or(0),
                likes: r.get("likes").and_then(|d| d.as_u64()).unwrap_or(0),
            })
        })
        .collect())
}

/// Reads a repository response and keeps its GGUF files, largest last.
pub fn parse_files(json: &str, total_memory: u64) -> Result<Vec<RepoFile>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("bad repository response: {e}"))?;
    let siblings = v
        .get("siblings")
        .and_then(|s| s.as_array())
        .ok_or("repository response lists no files")?;
    let mut files: Vec<RepoFile> = siblings
        .iter()
        .filter_map(|s| {
            let name = s.get("rfilename")?.as_str()?.to_string();
            if !name.to_lowercase().ends_with(".gguf") {
                return None;
            }
            let lfs = s.get("lfs");
            let size = lfs
                .and_then(|l| l.get("size"))
                .or_else(|| s.get("size"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let sha256 = lfs
                .and_then(|l| l.get("sha256"))
                .and_then(|h| h.as_str())
                .unwrap_or("")
                .to_lowercase();
            Some(RepoFile {
                quantization: crate::models::gguf::quantization_from_name(&name)
                    .unwrap_or_default(),
                fit: fit(size, total_memory).to_string(),
                name,
                size,
                sha256,
            })
        })
        .collect();
    files.sort_by_key(|f| f.size);
    Ok(files)
}

/// Whether a model of `size` bytes fits this machine: the graphics chip
/// shares system memory, so the file plus working room must leave the
/// rest of the desktop alone.
pub fn fit(size: u64, total_memory: u64) -> &'static str {
    if total_memory == 0 {
        return "unknown";
    }
    let share = size as f64 / total_memory as f64;
    if share <= 0.45 {
        "fits"
    } else if share <= 0.65 {
        "tight"
    } else {
        "too big"
    }
}

/// This machine's memory in bytes, from `/proc/meminfo`.
pub fn total_memory() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// `1.2 GB`, `640 MB`.
pub fn human_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else if bytes >= 1_000_000 {
        format!("{} MB", bytes / 1_000_000)
    } else {
        format!("{} KB", bytes / 1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_and_file_listings_parse_like_the_real_site() {
        let search = r#"[{"_id":"x","id":"unsloth/Qwen3.5-4B-GGUF","likes":453,"private":false,"downloads":1056719,"tags":["gguf"],"modelId":"unsloth/Qwen3.5-4B-GGUF"},{"id":"other/Thing-GGUF","likes":1,"downloads":2}]"#;
        let repos = parse_search(search).unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].id, "unsloth/Qwen3.5-4B-GGUF");
        assert_eq!(repos[0].downloads, 1_056_719);
        let files_json = r#"{"id":"unsloth/Qwen3.5-4B-GGUF","siblings":[
            {"rfilename":".gitattributes","blobId":"a","size":3060},
            {"rfilename":"Qwen3.5-4B-BF16.gguf","blobId":"b","size":8424393632,"lfs":{"sha256":"9E6E2841A75F503CCB330831832FD7861266E187E0DBF149A954219CCB8C197A","size":8424393632,"pointerSize":135}},
            {"rfilename":"Qwen3.5-4B-Q4_K_M.gguf","blobId":"c","size":2579944608,"lfs":{"sha256":"ff5c3e9740a5aa53f04fdf3b0b8cc75da556bf8948cdb19d61c512d3a43465d9","size":2579944608}}
        ]}"#;
        let files = parse_files(files_json, 32 * 1_000_000_000).unwrap();
        assert_eq!(files.len(), 2, "only GGUF files");
        assert_eq!(files[0].name, "Qwen3.5-4B-Q4_K_M.gguf");
        assert_eq!(files[0].quantization, "Q4_K_M");
        assert_eq!(files[0].fit, "fits");
        assert_eq!(
            files[0].sha256,
            "ff5c3e9740a5aa53f04fdf3b0b8cc75da556bf8948cdb19d61c512d3a43465d9"
        );
        assert_eq!(
            files[1].sha256,
            "9e6e2841a75f503ccb330831832fd7861266e187e0dbf149a954219ccb8c197a"
        );
        assert_eq!(fit(8_424_393_632, 32_071_680 * 1024), "fits");
        assert_eq!(fit(18_000_000_000, 32_000_000_000), "tight");
        assert_eq!(fit(25_000_000_000, 32_000_000_000), "too big");
        assert_eq!(human_size(2_579_944_608), "2.6 GB");
        assert!(search_url("qwen 4b", 20).contains("search=qwen%204b"));
        assert!(
            download_url("unsloth/Qwen3.5-4B-GGUF", "Qwen3.5-4B-Q4_K_M.gguf")
                .ends_with("/unsloth/Qwen3.5-4B-GGUF/resolve/main/Qwen3.5-4B-Q4_K_M.gguf")
        );
    }
}
