//! The `balerix` binary `scripts/operator.sh` built, and a temp root
//! under `target/tmp`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// From `BALERIX_BIN`; `None` after printing a skip (a failure under
/// `BALERIX_REQUIRE_TOOLS=1`).
pub fn balerix() -> Option<PathBuf> {
    let found = std::env::var_os("BALERIX_BIN")
        .map(PathBuf::from)
        .filter(|p| p.is_file());
    if found.is_none() {
        if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
            panic!(
                "BALERIX_BIN missing and BALERIX_REQUIRE_TOOLS=1 (run through scripts/operator.sh)"
            );
        }
        eprintln!("skip: BALERIX_BIN missing");
    }
    found
}

pub fn temp_root(label: &str) -> PathBuf {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}
