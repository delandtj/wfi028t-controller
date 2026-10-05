//! Stamps the firmware with the commit it was built from.
//!
//! `FW_VERSION` is `<crate version>+<commit>`, with `-dirty` appended when the
//! sources that go into the image (this crate, `hp-model`, the workspace
//! manifest and lock file) differ from that commit. Edits elsewhere in the
//! repo, docs for instance, do not count. Outside a git checkout it is the
//! crate version alone.
//!
//! The hello line, `status` (`fw=`) and the Home Assistant device's
//! `sw_version` all report it, so a build can be told apart from the next.

use std::process::Command;

/// Paths, relative to this crate, whose changes make a build `-dirty`.
const SOURCES: &[&str] = &[".", "../hp-model", "../Cargo.toml", "../Cargo.lock"];

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let crate_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let version = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(commit) => {
            let mut args = vec!["status", "--porcelain", "--untracked-files=no", "--"];
            args.extend_from_slice(SOURCES);
            let dirty = git(&args).is_some_and(|changes| !changes.is_empty());
            format!(
                "{crate_version}+{commit}{}",
                if dirty { "-dirty" } else { "" }
            )
        }
        None => crate_version,
    };
    println!("cargo:rustc-env=FW_VERSION={version}");

    // Re-stamp on a commit, a checkout, a staging change, or a source edit.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for path in ["HEAD", "index", "refs", "packed-refs"] {
            println!("cargo:rerun-if-changed={git_dir}/{path}");
        }
    }
    for path in [
        "src",
        "Cargo.toml",
        "../hp-model/src",
        "../hp-model/Cargo.toml",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
}
