# Apus WebGL2 demo

Build on Linux with Rust's `wasm32-unknown-unknown` target and the matching `wasm-bindgen` CLI.
The build checks `hg002-web-demo-v3.tar.gz` against the committed checksum before embedding
the data in WASM.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --locked --version 0.2.130
apus-web/build.sh /path/to/hg002-web-demo-v3.tar.gz
python3 -m http.server --bind 127.0.0.1 --directory apus-web/dist 8000
```

Open `http://127.0.0.1:8000/` in a WebGL2-capable browser. Click or tap a VCF row, or press Enter
on the selected row, to open its pileup. Drag the pileup or use the arrow keys to pan. Click
or hold in Local Area to navigate the reference slice. Use `+` and `-` to zoom and Tab or
the tab bar to switch views.

The release asset is `hg002-web-demo-v3.tar.gz` on the repository's `hg002-web-demo-v3` release.
To create it locally from the indexed hg38 FASTA, the indexed public ONT HG002 BAM and the
two-variant `ex-hg002` VCF,
run `apus-web/extract-demo.sh HG002_DATA_DIR hg002-web-demo-v3.tar.gz` with `samtools` on PATH.
Version 3 uses all alignments overlapping the two embedded reference slices, so Local Area
can show depth and indels within those slices. Rebuilding the pinned asset requires the same
inputs and samtools 1.25.

The archive includes the [data credits](data-credits.txt), [ONT MIT license](ONT-MIT-LICENSE.txt)
and [NIST data notice](../testdata/hg002/NIST-DATA-NOTICE.txt). The Pages build ships those files
with the [Apus license](../LICENSE.md), [Slug MIT notice](../apus-desktop/src/shaders/Slug-MIT-LICENSE.txt)
and [Fira Code OFL notice](../src/grid/fonts/LICENSE).

## GitHub Actions

Before the first run in a new repository, create the `hg002-web-demo-v3` release from its default
branch and attach the archive that matches `hg002-web-demo-v3.sha256`.

Run **Build the WebGL2 demo** from the repository's Actions tab. Leave **Deploy to GitHub Pages**
unchecked and download **github-pages** from the run's Artifacts section. Unzip the download and extract
`artifact.tar` to inspect or serve the built site locally.

To test deployment, set **Settings → Pages → Source** to **GitHub Actions** and rerun the
workflow with **Deploy to GitHub Pages** checked.

## Chromium smoke test

```sh
python3 -m venv /tmp/apus-web-smoke
/tmp/apus-web-smoke/bin/pip install playwright
/tmp/apus-web-smoke/bin/python -m playwright install --with-deps --no-shell chromium
APUS_WEB_SMOKE=1 apus-web/build.sh /path/to/hg002-web-demo-v3.tar.gz
/tmp/apus-web-smoke/bin/python apus-web/smoke.py
```

The smoke build exposes the rendered cell text for Chromium to check coordinates and depth.
The test serves the demo under a project path and uses SwiftShader with mouse and emulated
touch input. Build without `APUS_WEB_SMOKE` for Pages.
