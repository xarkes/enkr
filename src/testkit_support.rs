//! `CdpDriver` launcher shared by every `driver_test!` invocation (see
//! `driver_test!` in `src/lib.rs`) — one copy instead of one per test file.
//!
//! The wasm bundle these drivers load is a *build artefact*, and nothing in the
//! normal `cargo test` graph depends on it. Left to itself it goes stale
//! silently: every `::cdp` test keeps passing against a bundle built from older
//! source, which is worse than no coverage because it looks like coverage. So
//! the launcher rebuilds it on demand, once per test process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Rebuild outcome, computed once per test process and shared by every cdp test
/// in it (they run in parallel threads, and the build must not race itself).
static HARNESS: OnceLock<Result<(), String>> = OnceLock::new();

/// The crate root — both the cargo workspace the wasm harness is built in and
/// the directory `CdpDriver` serves over HTTP (so a page can reach `www/pkg/`
/// and `www/assets/`).
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Build `src/bin/test_harness.rs` to wasm and run `wasm-bindgen` over it —
/// the same two steps as `www/build_test_harness.sh`, in-process so it
/// works without a shell and so a failure surfaces as a test failure with the
/// real compiler output rather than a stale-bundle pass.
fn build_harness() -> Result<(), String> {
    let root = crate_root();
    let wasm = root.join("target/wasm32-unknown-unknown/debug/test_harness.wasm");
    let out_dir = root.join("www/pkg");
    let bundle = out_dir.join("test_harness_bg.wasm");

    // Safe to invoke cargo from here: by the time a test body runs, the outer
    // `cargo test` has finished building and released the target-directory
    // lock. (It is *not* safe from a build script, which runs while that lock
    // is held.)
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        .current_dir(&root)
        .args([
            "build",
            "--target",
            "wasm32-unknown-unknown",
            "--bin",
            "test_harness",
        ])
        .status()
        .map_err(|err| format!("could not run `{cargo} build` for the wasm harness: {err}"))?;
    if !status.success() {
        return Err(format!(
            "building the wasm test harness failed ({status}).\n\
             Needs `rustup target add wasm32-unknown-unknown`."
        ));
    }

    // wasm-bindgen is the slow half and only matters when the wasm actually
    // changed, so skip it when the bundle is already newer than its input.
    if is_fresh(&bundle, &wasm) {
        return Ok(());
    }

    let status = Command::new("wasm-bindgen")
        .current_dir(&root)
        .arg(&wasm)
        .args(["--out-dir"])
        .arg(&out_dir)
        .args(["--target", "web", "--no-typescript"])
        .status()
        .map_err(|err| {
            format!(
                "could not run `wasm-bindgen`: {err}.\n\
                 Install it with `cargo install wasm-bindgen-cli` at the version \
                 matching Cargo.toml's `wasm-bindgen` dependency."
            )
        })?;
    if !status.success() {
        return Err(format!("wasm-bindgen failed ({status})"));
    }
    Ok(())
}

/// Is `out` newer than `input`? `false` whenever either timestamp is
/// unavailable, so an unknown state rebuilds rather than trusting a stale file.
fn is_fresh(out: &Path, input: &Path) -> bool {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (mtime(out), mtime(input)) {
        (Some(out), Some(input)) => out >= input,
        _ => false,
    }
}

fn ensure_harness_built() {
    if let Err(err) = HARNESS.get_or_init(build_harness) {
        panic!("{err}");
    }
}

pub fn launch_test_harness() -> mae::testkit::cdp::CdpDriver {
    launch_test_harness_with_query("")
}

/// The harness at a given viewport size — what `driver_test!` uses, so a
/// scenario's declared width/height means the same thing on both backends
/// (it used to reach `NativeDriver` only, and this side ran at whatever size
/// Chrome happened to launch at).
pub fn launch_test_harness_sized(width: f32, height: f32) -> mae::testkit::cdp::CdpDriver {
    let mut driver = launch_test_harness();
    driver.set_viewport(width, height);
    driver
}

/// The harness with a query string appended, e.g.
/// `"?server=ws://127.0.0.1:9070/ws&nick=bob"`.
///
/// The web build deliberately offers no custom-server field (only the one
/// hardcoded default — see `EnkrState::add_server`), so a browser client has
/// no way through its own UI to reach a test relay. The harness binary reads
/// these two parameters instead and connects on startup; see
/// `src/bin/test_harness.rs`. Test-only, exactly like the harness itself.
pub fn launch_test_harness_with_query(query: &str) -> mae::testkit::cdp::CdpDriver {
    ensure_harness_built();
    let root = crate_root();
    mae::testkit::cdp::CdpDriver::launch(&root, &format!("/www/test_harness.html{query}"))
}

/// Launches the *real* deployed web app (`src/main.rs`'s wasm entry point,
/// as served from `www/`) rather than the fixture harness above.
///
/// Needed by anything testing persistence: the harness seeds
/// `NoteDatabase::demo()`, which has no store behind it at all, so nothing
/// it does ever reaches IndexedDB. Run `www/build.sh` first — unlike the
/// harness this is not rebuilt automatically, because it is the shipping bundle
/// and building it is the deploy step, not a test step.
pub fn launch_web_app() -> mae::testkit::cdp::CdpDriver {
    let root = crate_root();
    let pkg = root.join("www/pkg/enkr.js");
    assert!(pkg.exists(), "run www/build.sh first");
    mae::testkit::cdp::CdpDriver::launch(&root, "/www/")
}
