#!/usr/bin/env bash
# Builds the wasm/DOM web version of enkr. Requires:
# `rustup target add wasm32-unknown-unknown` and `cargo install wasm-bindgen-cli`
# (version must match the `wasm-bindgen` dependency in Cargo.toml) done
# once beforehand.
set -euo pipefail
cd "$(dirname "$0")/.."
RELEASE=1

if [ "$RELEASE" -ge "1" ]; then
  cargo build \
    --target wasm32-unknown-unknown \
    --bin enkr --release

  wasm-bindgen \
    target/wasm32-unknown-unknown/release/enkr.wasm \
    --out-dir www/pkg \
    --target web \
    --no-typescript
else
  cargo build \
    --target wasm32-unknown-unknown \
    --bin enkr 

  wasm-bindgen \
    target/wasm32-unknown-unknown/debug/enkr.wasm \
    --out-dir www/pkg \
    --target web \
    --no-typescript
fi

echo "Built. Serve the repo root (so www/ can reach www/assets/) and open /www/, e.g.:"
echo "  python3 -m http.server 8080"
echo "  open http://localhost:8080/www/"
