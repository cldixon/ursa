#!/usr/bin/env bash
# Build the threaded-layout spike (#503): two wasm builds of this crate into
# web/, and two native benches.
#
#   web/pkg-serial/    the layout as the shell ships it: the workspace's
#                      stable toolchain, no atomics, no rayon
#   web/pkg-threads/   the same source with `--features threads`, on the
#                      pinned nightly below, `+atomics,+bulk-memory` and a
#                      rebuilt std (`-Z build-std`)
#   bin/bench-serial, bin/bench-threads   the native benches (stable)
#
# Nothing here is part of the release build: this crate is a workspace of its
# own, and the nightly is named only on this script's cargo command line.
#
#   ./build.sh [serial|threads|native|all]        (default: all)
#
# Needs: rustup, wasm-bindgen 0.2.126 (the CLI the shell's build uses), and,
# for `threads`, network access to static.rust-lang.org the first time:
#   rustup toolchain install "$NIGHTLY" --profile minimal -c rust-src -t wasm32-unknown-unknown
set -euo pipefail
cd "$(dirname "$0")"

# The pin. To move it: change the date, run ./build.sh and `bun bench.ts
# check`, and re-read design/threaded-layout-spike.md's "what broke".
NIGHTLY="${NIGHTLY:-nightly-2026-11-01}"
# The shared memory's ceiling: a shared WebAssembly.Memory must declare its
# maximum, and the browser reserves that much address space when the module
# starts. 4 GB is wasm32's limit, and the unthreaded build's.
MAX_MEMORY="${MAX_MEMORY:-4294967296}"

TARGET="${CARGO_TARGET_DIR:-$PWD/target}"
what="${1:-all}"

bindgen() { # <wasm> <out dir>
  rm -rf "$2"
  wasm-bindgen --target web --no-typescript --remove-name-section --remove-producers-section --out-dir "$2" "$1"
}

if [[ $what == serial || $what == all ]]; then
  CARGO_TARGET_DIR="$TARGET/spike-serial" \
    cargo build --lib --target wasm32-unknown-unknown --profile wasm-release --locked
  bindgen "$TARGET/spike-serial/wasm32-unknown-unknown/wasm-release/ursa_viz_threads.wasm" web/pkg-serial
fi

if [[ $what == threads || $what == all ]]; then
  # --shared-memory and --import-memory make the module take its memory from
  # the glue, which makes one WebAssembly.Memory({shared: true}) and posts it
  # to each of the pool's workers; the TLS exports are what wasm-bindgen's
  # thread start-up reads.
  CARGO_TARGET_DIR="$TARGET/spike-threads" \
  RUSTFLAGS="-C target-feature=+atomics,+bulk-memory \
    -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=$MAX_MEMORY \
    -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size \
    -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base" \
    cargo "+$NIGHTLY" build --lib --features threads --target wasm32-unknown-unknown \
      -Z build-std=std,panic_abort --profile wasm-release --locked
  bindgen "$TARGET/spike-threads/wasm32-unknown-unknown/wasm-release/ursa_viz_threads.wasm" web/pkg-threads
fi

if [[ $what == native || $what == all ]]; then
  mkdir -p bin
  CARGO_TARGET_DIR="$TARGET/spike-native-serial" cargo build --release --bin bench --locked
  cp "$TARGET/spike-native-serial/release/bench" bin/bench-serial
  CARGO_TARGET_DIR="$TARGET/spike-native-threads" cargo build --release --features threads --bin bench --locked
  cp "$TARGET/spike-native-threads/release/bench" bin/bench-threads
fi

for d in web/pkg-serial web/pkg-threads; do
  [[ -d $d ]] || continue
  raw=$(cat "$d"/*.wasm "$d"/*.js $(find "$d/snippets" -name '*.js' 2>/dev/null) | wc -c)
  gz=$(for f in "$d"/*.wasm "$d"/*.js $(find "$d/snippets" -name '*.js' 2>/dev/null); do gzip -9c "$f" | wc -c; done | paste -sd+ | bc)
  echo "$d: $raw bytes raw, $gz gzipped"
done
