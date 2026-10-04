//! Model files: reading their metadata and finding them on disk.

pub mod discover;
pub mod gguf;

pub use discover::{scan, Found, Locations, Scan, Skipped};
pub use gguf::GgufInfo;
