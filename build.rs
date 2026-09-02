use std::path::Path;
use std::process::Command;

/// Capture the current git commit so the binary can log which build it is.
fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ENKR_GIT_HASH={hash}");

    // Rebuild when the checked-out commit changes: HEAD itself (branch switch)
    // and the ref it points at (a new commit on the same branch).
    let git = Path::new("../.git");
    let head = git.join("HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Ok(contents) = std::fs::read_to_string(&head) {
            if let Some(reference) = contents.strip_prefix("ref:").map(str::trim) {
                println!("cargo:rerun-if-changed={}", git.join(reference).display());
            }
        }
    }
}
