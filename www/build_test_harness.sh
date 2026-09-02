#!/usr/bin/env bash
# NOTE: you no longer need to run this by hand. `cargo test` rebuilds the
# harness on demand — see `enkr/src/testkit_support.rs::launch_test_harness`,
# which runs these same two steps once per test process. This script is kept
# for building the bundle without running any tests.
#
# Builds src/bin/test_harness.rs — the wasm test-only entry point
# `enkr/tests/app_sync.rs`'s CdpDriver scenarios (clicking_a_note_selects_it_cdp
# et al) drive, not the real app; see that binary's module doc comment for
# why. Requires: `rustup target add wasm32-unknown-unknown` and `cargo
# install wasm-bindgen-cli` (version must match the `wasm-bindgen` dependency
# in enkr/Cargo.toml) done once beforehand.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build \
  --target wasm32-unknown-unknown \
  --bin test_harness

wasm-bindgen \
  target/wasm32-unknown-unknown/debug/test_harness.wasm \
  --out-dir www/pkg \
  --target web \
  --no-typescript

echo "Built. cargo test -p enkr --test app_sync --features cdp"
