//! Shared code for every Belvedere binary.

pub mod agent;
pub mod db;
pub mod engine;
pub mod ipc;
pub mod model_ipc;
pub mod models;
pub mod schedule;
pub mod think;
pub mod thunderbird;
pub mod tools;
pub mod watch;

/// The version every Belvedere binary reports, taken from the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Formats the one-line version banner a binary prints for `--version`.
pub fn version_line(binary_name: &str) -> String {
    format!("{binary_name} {VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_is_name_then_version() {
        assert_eq!(version_line("belvedered"), format!("belvedered {VERSION}"));
    }

    #[test]
    fn version_matches_workspace_package() {
        assert_eq!(VERSION, "0.1.0");
    }
}
