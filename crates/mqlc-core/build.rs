//! Embeds the git commit in run manifests (`MQLC_GIT_SHA`), with `-dirty`
//! when the working tree has uncommitted changes.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    if let Some(sha) = git(&["rev-parse", "--short=12", "HEAD"]) {
        let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        let suffix = if dirty { "-dirty" } else { "" };
        println!("cargo:rustc-env=MQLC_GIT_SHA={sha}{suffix}");
    }
    // Re-run on checkouts, commits, staging and any edit under crates/, so
    // the sha and the dirty flag stay current for the code that gets built.
    for path in ["HEAD", "index", "refs"] {
        println!("cargo:rerun-if-changed=../../.git/{path}");
    }
    println!("cargo:rerun-if-changed=..");
}
