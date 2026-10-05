//! Running third-party parsers that may panic on odd input (some PDFs do)
//! without taking the caller down with them.

use std::panic::{catch_unwind, AssertUnwindSafe};

/// Runs `f`; a panic inside it becomes `None` instead of unwinding into
/// the caller. The panic message still reaches the log through the
/// default hook.
pub fn no_panic<T>(f: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(f)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_none_and_a_value_passes_through() {
        assert_eq!(no_panic(|| 7), Some(7));
        let v: Vec<u8> = Vec::new();
        assert_eq!(no_panic(|| v[0]), None);
    }
}
