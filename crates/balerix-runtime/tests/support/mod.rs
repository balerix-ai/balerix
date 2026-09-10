//! Shared helpers for the integration tests that drive real tools.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use balerix_runtime::{StateLayout, ToolPaths};

/// Tools from the test process's PATH (mise puts the pinned ones there); the
/// relay binary slot is filled with this test executable — it only has to
/// exist for the profile to validate.
pub fn tools() -> Option<ToolPaths> {
    let exe = std::env::current_exe().ok()?;
    ToolPaths::discover_in(&std::env::var_os("PATH").unwrap_or_default(), &exe).ok()
}

/// Returns `true` if the test may run. Otherwise prints a skip reason, or
/// panics when `BALERIX_REQUIRE_TOOLS=1` (CI never skips).
pub fn require_or_skip(name: &str, present: bool) -> bool {
    if present {
        return true;
    }
    if std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
        panic!("{name} is required (BALERIX_REQUIRE_TOOLS=1) but not available");
    }
    eprintln!("skip: {name} not available");
    false
}

/// A fresh directory under `target/tmp`. Deliberately not `/tmp`: nono's
/// built-in groups grant `/tmp`, so a sandbox-escape assertion there would
/// pass vacuously.
pub fn temp_root(test: &str) -> balerix_runtime::testing::TempRoot {
    balerix_runtime::testing::TempRoot::new(Path::new(env!("CARGO_TARGET_TMPDIR")), test)
}

pub fn layout(root: &Path) -> StateLayout {
    StateLayout {
        state_root: root.join("state"),
        data_root: root.join("data"),
        config_root: root.join("config"),
    }
}

/// True when Landlock is usable: `nono run` of `true` succeeds. `--allow-cwd`
/// is required because nono 0.75.0 refuses CWD access non-interactively
/// without it; without the flag the probe fails even though Landlock itself
/// is fine. `$HOME` must be a sibling of `root`, not nested inside it: nono's
/// built-in default profile denies a set of sensitive dotfiles under `$HOME`
/// (`.aws`, `.ssh`, `.1password`, …), and when `--allow-cwd` broadly allows a
/// directory that contains `$HOME`, those per-file denials sit underneath an
/// allowed parent — a shape Landlock cannot express, so nono refuses to start
/// ("Sandbox initialization failed: Landlock deny-overlap is not
/// enforceable") rather than silently under-enforcing.
pub fn landlock_works(tools: &ToolPaths, root: &Path) -> bool {
    let home = root.with_file_name(format!(
        "{}-nono-probe-home",
        root.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::create_dir_all(&home).unwrap();
    let result = std::process::Command::new(&tools.nono)
        .args(["-s", "run", "--allow-cwd", "--", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(&home);
    result
}
