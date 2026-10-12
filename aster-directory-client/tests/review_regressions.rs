//! Regressions from the 2026-10-12 CLI review. Expected strings are CPython
//! 3.13 `json.dumps` output; any divergence changes a contract ID or a
//! request signature.

use aster_directory_client::pyjson::{DumpOptions, PyJson};

/// `ensure_ascii=True` escapes everything outside printable ASCII
/// (`[^\ -~]`), which includes DEL (U+007F).
#[test]
fn ensure_ascii_escapes_del() {
    let v = PyJson::Str("a\u{7f}b".into());
    assert_eq!(
        v.dumps(DumpOptions::PYTHON_DEFAULT).unwrap(),
        r#""a\u007fb""#
    );
}

/// Python integers are unbounded; `json.dumps(12345678901234567890123)`
/// writes the digits, not a float.
#[test]
fn integers_beyond_u64_stay_integers() {
    let v = PyJson::parse("12345678901234567890123").unwrap();
    assert_eq!(
        v.dumps(DumpOptions::PYTHON_DEFAULT).unwrap(),
        "12345678901234567890123"
    );
}
