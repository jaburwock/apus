# Plan: Synthetic split-read generation

Goal: extend `src/testgen/` to generate split reads (`CigarKind::Skip`, `'N'`) at an exact
split locus and size, and add a committed `testdata/bam/medium_split30bp.bam` fixture with a
`cargo ex-medium-split` example alias. No changes to alignment display.

## Steps

1. **Split specification and seam (`src/testgen/mod.rs`, `src/lib.rs`)**
   - Add `pub struct SplitSpec<'a> { pub contig: &'a str, pub start: u64, pub len: u64 }`
     (1-based SAM coordinate for `start`, matching `fixed_locus`, `len > 0`).
   - Export `SplitSpec` under `#[cfg(any(test, feature = "testgen"))]` in `src/lib.rs`.
   - Extend `generate_sam_line` with `split: Option<SplitSpec<'_>>`.
   - For reads on `split.contig` with `pos < split.start`, roll 50/50 (`rng.random_bool(0.5)`);
     if true, pass `Some((split.start - pos, split.len))` to `gen_cigar`. Reads starting at or
     after `split.start` (including inside the split) stay unspliced.
   - Validate `split.len > 0` and contig bounds.
   - Update `gen_tiny_bam` and `gen_medium_bam` to pass `None`.

2. **Exact `N` injection in CIGAR walk (`src/testgen/cigar.rs`)**
   - Extend `gen_cigar` to accept `split: Option<(u64, u64)>` (`(ref_offset, skip_len)`).
   - Track `ref_used` alongside `query_used`.
   - When `split` is pending, `ref_used == ref_offset`, and `query_used > 0` (while
     `query_used < read_len`), emit `CigarKind::Skip` of length `skip_len`.
   - Before the split is emitted, resample `Deletion(len)` if `query_used == 0` or
     `ref_used + len > ref_offset` so deletions cannot cross the splice boundary or precede
     the first aligned base.
   - Because `Skip` consumes 0 query bases, `query_used == read_len` and `mismatch_offsets`
     stay aligned automatically.

3. **Exon-segment FASTA fetching (`src/testgen/seq.rs`)**
   - Refactor `gen_seq` to fetch only non-`Skip` reference segments instead of one contiguous
     span covering skipped intron bases.

4. **Medium 30bp split fixture and example (`src/bin/gen_medium_split30bp_bam.rs`, `Cargo.toml`, `.cargo/config.toml`, `testdata/README.md`)**
   - Adapt `gen_medium_bam` to generate `testdata/bam/medium_split30bp.bam` and `.bai` with
     `SplitSpec { contig: "chr1", start: 32, len: 30 }`.
   - Add `[[bin]]` entry in `Cargo.toml` (`required-features = ["testgen"]`).
   - Add `ex-medium-split` alias in `.cargo/config.toml`.
   - Document the fixture in `testdata/README.md`.

5. **Foundational tests & verification**
   - Property/round-trip tests in `testgen/cigar.rs`, `testgen/seq.rs`, and `testgen/mod.rs`.
   - Run `cargo fmt`, `cargo clippy --all-targets --all-features`, `cargo nextest run`, and
     `cargo test`.
   - Delete this plan file and its link in `src-wiki/data.md` before pausing for review.
