#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 ]]; then
    echo "Usage: $0 hg002-web-demo-v3.tar.gz" >&2
    exit 2
fi
archive=$(realpath "$1")
cd "$(dirname "$0")"
if ! echo "$(cat hg002-web-demo-v3.sha256)  $archive" | sha256sum --check --status; then
    echo "Demo data checksum does not match hg002-web-demo-v3.sha256" >&2
    exit 1
fi
if [[ $(wasm-bindgen --version) != "wasm-bindgen 0.2.130" ]]; then
    echo "This build requires wasm-bindgen-cli 0.2.130" >&2
    exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
tar -xzf "$archive" -C "$stage"
features=()
if [[ ${APUS_WEB_SMOKE:-0} == 1 ]]; then
    features=(--features smoke)
fi
target_dir=${CARGO_TARGET_DIR:-target}
APUS_DEMO_DATA="$stage" cargo build --locked --release --target wasm32-unknown-unknown --target-dir "$target_dir" "${features[@]}"

mkdir -p dist
wasm-bindgen --target web --no-typescript --out-dir dist --out-name apus_web \
    "$target_dir/wasm32-unknown-unknown/release/apus_web.wasm"
cp index.html main.js dist/
cp "$stage"/{data-credits.txt,ONT-MIT-LICENSE.txt,NIST-DATA-NOTICE.txt} dist/
cp ../apus-desktop/src/shaders/Slug-MIT-LICENSE.txt dist/
cp ../src/grid/fonts/LICENSE dist/FiraCode-OFL.txt
cp ../LICENSE.md dist/Apus-LICENSE.md
