# Apus

> [!IMPORTANT]
> **Early alpha preview.** Apus is under active development. Expect bugs and changes to the
> interface, commands, and configuration.

A lightweight terminal viewer for BAM read alignments and VCF variants on HPC Linux systems, built
with Rust, ratatui, and noodles. Jump from a variant to its genomic locus and inspect the aligned
reads in a reference-backed pileup that shows CIGAR operations and mismatches.

[Download a standalone executable](https://github.com/jaburwock/apus/releases) or
[try the HG002 demo in your browser](https://jaburwock.github.io/apus/).

## Quickstart

Choose `apus` for the terminal or `apus-desktop` for the desktop, then pick your platform.
On Windows, run the downloaded `.exe`. On macOS and Linux, set the execute permission first.
For example, on Linux x64, run `chmod +x apus-linux-x64 && ./apus-linux-x64`.

To build from source, run these commands from the repository root.

```sh
cargo build --release
cargo run
```

Click **Genome** or **VCF** in the tab bar to switch views. Press Tab to cycle through them.
Click **Settings (,)**, press `,`, or run `:settings` (also `:help`) to see live settings and the
command reference. Left-click a boolean to toggle it or a number to edit it. Enter activates the
selected row or applies an edit. Press Esc or left-click away from the edited row to discard the
draft. Click **Hotkeys (?)**, press `?`, or run `:shortcuts` to see the keys for the current view.
Hover over a row to select it in Settings, Hotkeys, or the genome track picker. Long lists scroll
as the selection approaches either edge.

In the genome view, left-click and drag the pileup to pan in any direction. Drag along the
chromosome bar to skim the contig. Hold the left mouse button on either side of the local-area
chart to keep jumping in that direction, or drag to steer.

Use `:set localareareadspans on` to shade the share of local-area coverage contributed by
reads crossing the pileup centreline, including reads offscreen in the pileup. The shading
uses aligned reference spans to estimate depth in the coverage chart.

Pileup coverage shows insertion support at the median position of each cluster that reaches
`:set allelefraction`. A cluster spans up to 5 bp, and its shortest and longest insertions
differ by at most 10% of the larger size. The insertion-colored portion of the bar shows
the share of reads supporting the cluster.

Pass multiple BAM paths on the command line to open the first and list the others. Press **B**
(or type `:bams`) to see which BAMs and companion `.bam.bai` files are present. Use ↑ and ↓
to select a BAM and Enter to switch at the current locus. The active filename appears at the
bottom left of the contig grid.

### Startup config

Apus reads one UTF-8 command file. It uses `--config PATH` when supplied. Otherwise it looks
for `.apusconfig` in the working directory, then `~/.apusconfig`. The first file takes precedence,
even if it is empty. A selected file that cannot be read is reported as an error.

Write one command per line. Lines beginning with `#` are comments. Relative data paths are
resolved from the working directory. The config runs after the saved palette and before
positional file arguments, so a BAM chosen on the command line takes precedence.

```text
set activebam "/data/sample A.bam"
addbam "/data/sample B.bam"
```

### Desktop and examples

To build and run the GPU-rendered desktop window instead of the terminal, use
`cargo run --release -p apus-desktop`. Hold Ctrl and scroll up or down to resize the font, or use
Ctrl+Plus and Ctrl+Minus.

These examples use fixtures included in a fresh clone.

```sh
cargo ex-tiny         # Small read pileup against the bundled reference subset
cargo ex-medium       # Larger read pileup against the same reference subset
cargo ex-vcf-clinvar  # ClinVar variant table using the bundled subset
cargo ex-hg002        # Two HG002 structural variants and their read pileups
cargo ex-hg002-multisample  # Three HG002 loci in one VCF with three matching BAMs
```

Use `:set colorscheme red-green dark` for the dark palette tuned for common red-green color vision
deficiencies. You can also choose it under **Select palette** in the palette menu (`:palette`).

## Optional large data

Choose a data directory on a disk with room for the downloads.

```sh
data="$HOME/apus-data"
bash testdata/fetch.sh "$data" gtf      # cargo ex-fullgtf
bash testdata/fetch.sh "$data" ref gtf  # cargo ex-full
bash testdata/fetch.sh "$data" clinvar  # cargo ex-vcf-clinvar-full
```

The script uses `curl` or `wget` and needs `samtools` to index the reference. With Pixi installed,
`pixi run fetch "$data"` supplies the tools and fetches all groups. The script reuses existing
data in the supplied directory and links it into the gitignored `testdata/large/`.
[Test data manifest](testdata/README.md)

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md): the project invariants, conventions and module
index. The [wiki](src-wiki/) holds the constraints that span files.

Run [Build release binaries](.github/workflows/release-binaries.yml) from GitHub Actions. Leave
`release_tag` blank to download executables, checksums and license notices from the run. To create a draft
prerelease, select the ref to build and enter a tag such as `v0.1.0-alpha.1`. The draft targets
the built commit and includes eight `apus` and `apus-desktop` executables, four SHA-256 checksum
files and the license notices. Open the draft from the workflow summary and publish it after review.

## License and credits

Apus is distributed under the [MIT License](LICENSE.md). The example data have their own
[credits and terms](testdata/README.md#fixture-credits), including the [bundled HG002 examples](testdata/README.md#bundled-hg002-examples).

Include the Apus license with binary distributions. Desktop and web builds also include Slug
shader code and embed Fira Code. Ship their [Slug MIT notice](apus-desktop/src/shaders/Slug-MIT-LICENSE.txt)
and [Fira Code OFL notice](apus-desktop/assets/fonts/LICENSE) with those builds.
