//! Reading mail from Thunderbird's mbox files: splitting a file into
//! messages, turning each into a small normalized record, and scanning
//! folders so only new bytes are read.

pub mod mbox;
pub mod parse;
pub mod scan;

pub use parse::{normalize, Normalized};
pub use scan::{scan_folder, ScanOutcome};
