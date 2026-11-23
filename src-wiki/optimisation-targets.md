# Optimisation Targets

Measured baselines and ranked optimisation targets from `prof_genome` against the full-scale
HG002 ONT 5mC modBAM (`HG002_public_5kHz_sup_v5.2r2_5mC.bam`, 133 GiB) and `hg38.fa` on a
4-vCPU Linux VM (warm OS page cache).

## Reproducing

Run individual workloads when iterating (the full `all` suite at baseline took several minutes
because `pileup-classify` alone spent ~40–60 ms/frame × 200 frames × 3 modes per locus):

```sh
# Single-workload timing check
cargo prof-hg002 --pos 152304195 --span 500000 --workload pileup-classify --frames 40 --iters 3

# Single-workload flamegraph (requires perf_event_paranoid <= 1 and `cargo install flamegraph`)
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling --features profiling --bin prof_genome
perf record -e cpu-clock -F 997 --call-graph fp -o target/flamegraphs/out.perf.data -- \
  target/profiling/prof_genome --bam testdata/large/hg002/HG002_public_5kHz_sup_v5.2r2_5mC.bam \
  --fasta testdata/large/hg002/hg38.fa --pos 152304195 --span 500000 --workload local-area-5mc --warmup 1 --iters 2
flamegraph --perfdata target/flamegraphs/out.perf.data -o target/flamegraphs/out.svg
```

## Baseline Measurements (2026-11-21)

Measured before the pileup classification, reference scanning and BAM fetch changes below.

BAI + header load: ~128 ms (paid once on BAM open via `SharedIndex`).

| Workload | `chr1:108.19M` 100 kb (~35×) | `chr1:108.19M` 500 kb (~32×) | `chr1:152.30M` 500 kb (~55×) | `chr1:148.55M` 500 kb (~61×) | Notes (`152.30M` 500 kb) |
|---|---:|---:|---:|---:|---|
| `bgzf-floor-single` | 92.7 ms | 176.1 ms | 302.2 ms | 266.3 ms | 1,427 recs, 49.3 MiB (163 MiB/s) |
| `bgzf-floor-chunked` | 1,121.9 ms | 1,384.7 ms | 2,900.0 ms | 2,056.8 ms | 16 chunks, 2,211 recs, 88.3 MiB (30 MiB/s) |
| `local-area-cov` | 1,119.3 ms | 1,385.2 ms | 2,898.7 ms | 2,066.5 ms | 489 bins, 52 indel bins |
| `local-area-5mc` (`min_cpgs=10`) | 1,154.8 ms | 1,509.0 ms | 3,153.4 ms | 2,345.8 ms | 101/489 bins qualify, 120k 5mC calls |
| `local-area-5mc` (`min_cpgs=1`) | — | — | 3,276.2 ms | — | 483/489 bins qualify, 208k 5mC calls |
| `pileup-fetch` (8 × 2 kb) | 787.0 ms | 791.6 ms | 1,433.8 ms | 1,118.7 ms | 424 reads; heap 15.4 MiB (30.0 MiB @ 99×) |
| `pileup-classify-close` | 39.68 ms/f | 55.68 ms/f | 57.76 ms/f | 59.84 ms/f | 40 reads @ 200 cols (1 bp/col) |
| `pileup-classify-mid` | 39.76 ms/f | 55.76 ms/f | 57.91 ms/f | 60.00 ms/f | 40 reads @ 200 cols (10 bp/col) |
| `pileup-classify-mid-5mc` | 39.84 ms/f | 55.90 ms/f | 58.23 ms/f | 60.21 ms/f | +0.3 ms/f over non-5mC |
| `local-area-draw-cov` | 0.144 ms/f | 0.146 ms/f | 0.147 ms/f | 0.148 ms/f | Breakpoints + wedges + quadrants |
| `local-area-draw-5mc` | 0.038 ms/f | 0.039 ms/f | 0.042 ms/f | 0.040 ms/f | Full-column 5mC chart |
| `ref-cpg-band-draw` | 1.103 ms/f | 5.535 ms/f | 5.464 ms/f | 5.509 ms/f | 200 cols, 4,021 CpGs/frame |

Flamegraph self-time breakdown (`chr1:152.30M`, 500 kb):
- `local-area-cov`: **93.6% `zlib_rs`** (inflate + CRC32); `Record::clone`, CIGAR walk, and indel binning combined are <2%.
- `local-area-5mc` (`min_cpgs=1`): **83.3% `zlib_rs`**, 3.0% `noodles_bam` SEQ unpack, 2.5% `accumulate_5mc`.
- `pileup-fetch`: **88.2% `zlib_rs`**, 1.6% SEQ unpack. Heap split across 27×–99× (13.0–30.0 MiB for 8 × 2 kb windows): **~76% `seq`**, **~14% `mod_5mc`**, **~9% `cigar`**.
- `pileup-classify`: **>90% `ReferenceData`** (`lookup` 31.4%, `insert_mru` 17.5%, `base_at` 6.1%, `request_chunk` 5.2%, plus ~30% libc `memmove`/`malloc`).
- `local-area-draw`: **64.9% `ReferenceLookup::column_bases`** (5.5 ms/frame), while chart rasterisation itself is <0.15 ms/frame.

## Ranked Targets

### 1. Per-frame pileup classification & `ReferenceData::lookup` MRU churn — **Landed**
- **Root cause:** `LaneCoverage::classify_read_against` walked every base of every visible read (tens of kilobases per ONT read) and called `reference.base_at(bp)` *before* checking whether `bp` lay in `[left, left + bp_width)`. Meanwhile `ReferenceData::lookup` removed and re-inserted at index 0 (`Vec::remove` + `Vec::insert`) on every single base even on a slot-0 hit, and `request_chunk` allocated `contig.to_string()` before checking residency/pending and triggered off-screen reference fetches.
- **Fix:** Clipped CIGAR ops in `classify_read_against` to `[left, left + bp_width)` (breaking once `ref_cursor > view_end`), advanced 5mC calls with `partition_point`, replaced `remove`+`insert_mru` with `rotate_right(1)` only when `index > 0`, and moved `contig.to_string()` after the residency/pending check.
- **Measured (`chr1:152.30M`, 500 kb, 40 reads @ 200 cols):**
  - `pileup-classify-close`: **57.76 → 0.130 ms/frame** (444×)
  - `pileup-classify-mid`: **57.91 → 1.123 ms/frame** (51.6×)
  - `pileup-classify-mid-5mc`: **58.23 → 1.376 ms/frame** (42.3×)

### 2. Per-frame reference & CpG band scan — **Landed**
- **Root cause:** `draw_area_reference` called `ReferenceLookup::column_bases` across all 200 columns every frame, rescanning up to 500 kb of reference bytes (`count_bases` + `windows(2)` CpG scan) and churning the MRU list on every column.
- **Fix:** Added 64-bp block-strided prefix sums (`acgt`, `invalid`, `cpg`) directly on `Chunk` inside `ReferenceData` (obeying the single-reference-cache rule in `tracks.md`), hitting 100% even during continuous scrolling and speeding up `SummaryGrid::resolve_5mc_eligibility` too.
- **Measured (`chr1:152.30M`, 500 kb, 200 cols):**
  - `ref-cpg-band-draw`: **5.464 → 0.124 ms/frame** (44.1×, identical 4,021 CpGs/frame)

### 3. Repeated BAI queries in local-area and pileup fetches — **Landed**

Long ONT reads occupy high-level BAI bins, so adjacent narrow queries repeatedly decompress the
same upstream BGZF blocks. Local-area summaries now stream each contiguous missing run through
one query, emitting finalised bins as they become available. Pileup uses 8, 16, or 32 kb fetch
windows based on sampled read spans. Both readers stop at the end of the requested interval.

Median wall times on HG002 compared with the November 21 baseline. The local-area spans are
column headings. Pileup fetches cover 16 kb at each locus and use centres aligned to the
baseline's 2 kb grid.

| Workload | `chr1:108.19M` 100 kb | `chr1:108.19M` 500 kb | `chr1:152.30M` 500 kb | `chr1:148.55M` 500 kb |
|---|---:|---:|---:|---:|
| `local-area-cov` | 1,119.3 → 24.8 ms | 1,385.2 → 101.0 ms | 2,898.7 → 177.7 ms | 2,066.5 → 185.5 ms |
| `local-area-5mc` (`min_cpgs=10`) | 1,154.8 → 43.6 ms | 1,509.0 → 200.4 ms | 3,153.4 → 360.2 ms | 2,345.8 → 391.8 ms |
| `pileup-fetch` (16 kb) | 787.0 → 62.4 ms | 791.6 → 62.4 ms | 1,433.8 → 48.7 ms | 1,118.7 → 97.8 ms |

BGZF stays single-threaded under the [HPC worker constraint](architecture.md#stack).
The pinned multithreaded reader could abort when a query abandoned prefetched blocks.
A temporary fix sped up 500 kb coverage and 5mC scans by 2.56× and 1.88× on four vCPUs
with 1.5–2.7 MiB more peak process RSS across the benchmark cases.

### 4. Packed BAM sequences, CIGAR, and streaming 5mC decoding — **Landed**

Pileup keeps BAM's 4-bit sequence encoding and 32-bit CIGAR ops to offset the cost of caching
wider windows. The shared 5mC decoder streams raw auxiliary tags and reads CpG context directly
from packed sequence. At `chr1:152.30M`, a 16 kb pileup request now uses one 32 kb window, with
136 cached read copies and 2.42 MiB of reported heap, down from 424 copies and 15.4 MiB.
