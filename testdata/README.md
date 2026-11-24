# Test data

Run the regeneration commands from the repository root. Tests use only the committed fixtures; the
optional downloads serve the full-scale examples and fixture regeneration.

## Committed files

| File | Purpose | How to regenerate |
| --- | --- | --- |
| `fetch.sh` | Downloads and checks optional full-size files and indexes the FASTA. | Maintained as a shell script. |
| `large.sha256` | Checksums for the four downloaded files, checked by `fetch.sh`. | Compute SHA-256 from the original upstream archives and index when updating the sources. |
| `build_hs1_subset.sh` | Extracts roughly the first 0.1% of each hs1 contig, retaining all of chrM. | Maintained as a shell script. |
| `reference/hs1_subset.fa` | Small hs1 reference for the tests, both BAM generators, `ex-tiny` and `ex-medium`. | Fetch `ref`, then run `bash testdata/build_hs1_subset.sh`. |
| `reference/hs1_subset.fa.fai` | Its FASTA index. | Written with the FASTA by `build_hs1_subset.sh`. |
| `bam/tiny.bam` | Ten synthetic reads for the tests and `ex-tiny`. | `cargo run --features testgen --bin gen_tiny_bam`. |
| `bam/tiny.bam.bai` | Its index. | Written by `gen_tiny_bam`. |
| `bam/medium.bam` | Fifty longer synthetic reads for `ex-medium` and `ex-full`. | `cargo run --features testgen --bin gen_medium_bam`. |
| `bam/medium.bam.bai` | Its index. | Written by `gen_medium_bam`. |
| `gtf/gtf_subset.gtf.gz` | Hand-edited RefSeq subset for the GTF and genome-viewer tests. Includes a malformed coordinate. | See [Rebuilding the GTF subset](#rebuilding-the-gtf-subset). |
| `vcf/sample.vcf` | Hand-written shapes a VCF reader trips on. | Edit the plain VCF directly. |
| `vcf/sample.vcf.gz` | BGZF copy, checked against the plain file. | `cargo run --bin gen_sample_vcf`. |
| `vcf/sample.vcf.gz.tbi` | Its tabix index. | Written by `gen_sample_vcf`. |
| `vcf/raw_info.vcf` | INFO keys with no `##INFO` declaration, for the raw fallback. | Edit the plain VCF directly. |
| `vcf/unsorted.vcf` | Positions go backwards within one contig, for the line-index disorder test. | Edit the plain VCF directly. |
| `vcf/header_order_mismatch.vcf` | Regression fixture for contig ranking by record order. The header declares contigs by length, like Clair3, while records run in numeric contig order. | Edit the plain VCF directly and preserve the difference between header and record order. |
| `vcf/clinvar_subset.vcf.gz` | Every 57th record of the full ClinVar file, covering all 24 contigs for `ex-vcf-clinvar`. | Fetch `clinvar`, then run `cargo run --bin gen_clinvar_subset`. |
| `vcf/clinvar_subset.vcf.gz.tbi` | Its tabix index. | Written by `gen_clinvar_subset`. |
| `hg002/demo/` | Indexed FASTA, BAM and VCF for `ex-hg002`. | See [Rebuilding the HG002 example](#rebuilding-the-hg002-example). |
| `hg002/multisample/` | Indexed FASTA, four BAMs and one VCF for `ex-hg002-multisample`. | See [Rebuilding the HG002 example](#rebuilding-the-hg002-example). |

The BAM generators seed the record data but mint fresh UUIDs for read names, so a rerun always
produces a diff. Regenerate only when the fixture needs to change.

## Fixture credits

- **hs1 FASTA.** Slices of UCSC's hs1 download of the T2T Consortium's CC0 T2T-CHM13v2.0
  assembly, including a slice of the HG002 Y chromosome. [UCSC hs1 download](https://hgdownload.soe.ucsc.edu/goldenPath/hs1/bigZips/),
  [UCSC data terms](https://genome.ucsc.edu/license/), [T2T assembly and CC0 notice](https://github.com/marbl/CHM13#data-reuse-and-license),
  [Nurk et al.](https://doi.org/10.1126/science.abj6987), [Rhie et al.](https://doi.org/10.1038/s41586-023-06457-y)
- **hs1 GTF.** Selected and edited records from the NCBI RefSeq annotation distributed by UCSC as
  `hs1.ncbiRefSeq.gtf.gz`. [UCSC download](https://hgdownload.soe.ucsc.edu/goldenPath/hs1/bigZips/genes/),
  [NCBI RefSeq](https://www.ncbi.nlm.nih.gov/refseq/), [NCBI data policy](https://www.ncbi.nlm.nih.gov/home/about/policies/#molecular-data)
- **ClinVar VCF.** Every 57th record of NCBI ClinVar's 2024-06-24 export after Ensembl remapped it
  to T2T-CHM13v2.0 with CrossMap. NCBI places no restrictions of its own on redistribution of
  molecular data and notes that submitters may assert rights. Ensembl's terms also recognize
  third-party rights. [ClinVar at NCBI](https://www.ncbi.nlm.nih.gov/clinvar/),
  [ClinVar citation](https://doi.org/10.1093/nar/gkae1090),
  [NCBI data policy](https://www.ncbi.nlm.nih.gov/home/about/policies/#molecular-data),
  [Ensembl T2T VCF](https://ftp.ensembl.org/pub/rapid-release/species/Homo_sapiens/GCA_009914755.4/ensembl/variation/2022_10/vcf/2024_07/clinvar_20240624_GCA_009914755.4.vcf.gz),
  [Ensembl data terms](https://www.ensembl.org/about/articles/ensembl-content-disclaimer)

## Calibrating the synthetic BAM generators

The generators use aggregate counts from the public GRCh38
`HG002_public_5kHz_sup_v5.2r2_5mC.bam` listed in [sources.txt](hg002/sources.txt).
The BAM was accessed on 2026-11-23. The fixtures synthesize reads against the bundled hs1 reference.
[ONT Methylation Benchmarking Datasets](https://registry.opendata.aws/ont_basemod_data/),
[Kulkarni et al.](https://doi.org/10.1101/2024.11.09.622763)

SHA-256: `94caa6c05943500b2df9651562e9afb5cffa115ca46c4840962f572b00c70750`.
The local copy matched the public S3 object's composite CRC32 `hcteBQ==-8478`.

Run this with the full BAM fetched with `--bam` and indexed by `hg002/preprocess.sh`. The counts
below were measured with Python 3.14.4 and the locked samtools/HTSlib 1.25.

```sh
pixi run --locked python3 testdata/hg002/generator_stats.py \
  testdata/large/hg002/HG002_public_5kHz_sup_v5.2r2_5mC.bam
```

The script samples 198 windows, each 100,000 bases wide. On chr1–chr22, each window is centred at
10%, 20%, …, 90% of the contig length, rounded down using the BAM's `@SQ` lengths. It
keeps mapped, unpaired records whose leftmost alignment position falls in a window and measures
their full alignments. MAPQ 60 was the mode at 48,844 of 51,242 records. Those 48,844 records form
the calibration sample. They have FLAG 0 (24,655) or 16 (24,189).

- **Indels.** Count each CIGAR `I` or `D` run of length 1–300 once and group them by length.
  Divide 3,257,916 runs by the sum of those runs and 913,635,440 aligned bases (`M`, `=`, `X`)
  to estimate the probability that a generator step starts an indel. The count buckets are in
  [`src/testgen/cigar.rs`](../src/testgen/cigar.rs).
- **Mismatches.** Count 3,305,277 single-base changes from `MD`, skipping `^` deletion runs, and
  divide by the 913,635,440 aligned bases. The rate measures differences from GRCh38, which can
  include sample variation. [SAM tags specification](https://samtools.github.io/hts-specs/SAMtags.pdf)
- **Base quality.** Sum Phred scores across aligned and inserted bases with a present `QUAL`.
  This excludes clipped bases. The sum of 35,807,523,933 across 917,536,454 bases gives a mean
  of 39.026.

## Bundled HG002 examples

`cargo ex-hg002` opens a 2,477 bp deletion and a 975 bp insertion. Press Enter on either VCF row
to jump to its pileup. The bundled FASTA retains both hg38 windows at their original chr1
coordinates and uses `N` for the rest of chr1. The directory contains the
[data credits](hg002/demo/data-credits.txt) and [ONT dataset license](hg002/demo/ONT-MIT-LICENSE.txt).

`cargo ex-hg002-multisample` adds a 55 bp insertion on chr2. Each BAM holds the reads from one
locus. `ins55_rep.bam` duplicates the alignments in `ins55.bam` under a second mock sample, so
the chr2 row shows `2` in the Samples column. The VCF sample IDs and BAM read-group sample names
match the BAM filename stems.

| Locus (GRCh38) | `del2477` | `ins975` | `ins55` | `ins55_rep` |
| --- | --- | --- | --- | --- |
| chr1:108190703 | `1/1` | `./.` | `./.` | `./.` |
| chr1:152304195 | `./.` | `0/1` | `./.` | `./.` |
| chr2:130963905 | `./.` | `./.` | `1/1` | `1/1` |

The called GTs come from the HG002 benchmark with phase removed. `./.` marks loci outside each
BAM's coverage. See the [multisample data credits](hg002/multisample/data-credits.txt).

## Optional full-size data

Run `bash testdata/fetch.sh DATA_DIR [group ...]` to fetch any missing data and link it into the
gitignored `testdata/large/`. Existing files and indexes in `DATA_DIR` are reused. The script uses
curl or wget, verifies new downloads, and needs gzip and samtools to prepare `ref`. From the
repository root, `pixi run fetch DATA_DIR` supplies the tools and fetches all groups. Rust and Cargo
are installed separately via rustup. If upgrading an older checkout, move the files from
`testdata/large/` to your data directory first.

| Group | Files in `large/` | Examples and regeneration |
| --- | --- | --- |
| `ref` | UCSC `hs1.fa.gz`, decompressed to `hs1.fa` (~3 GB) and indexed as `hs1.fa.fai` | `cargo ex-full` with `gtf`. Input for `build_hs1_subset.sh`. |
| `gtf` | UCSC `hs1.ncbiRefSeq.gtf.gz` | `cargo ex-fullgtf` and, with `ref`, `cargo ex-full`. The app creates the sibling `.apuscache` on first load. |
| `clinvar` | Ensembl `clinvar_20240624_GCA_009914755.4.vcf.gz` and `clinvar_20240624_GCA_009914755.4.vcf.gz.tbi` | `cargo ex-vcf-clinvar-full`. Input for `gen_clinvar_subset`. |
| `all` | All of the above | All full-size examples. |

### HG002 GRCh38 example

Run from the repository root with the Pixi environment available. Use a data directory on a disk
with room for the inputs. Fetch reuses existing files, including a decompressed `hg38.fa`. Add
`--bam` after the directory to fetch the 133 GiB ONT BAM. The `testdata/large/hg002` symlink points
to the supplied directory, so its existing and newly generated files are accessible there.
Download URLs are in [`hg002/sources.txt`](hg002/sources.txt). A file named `hg002-sources.txt`
in the data directory can override them, one URL per line with the same filenames.

```sh
data="$HOME/apus-data"
pixi run --locked --no-install bash testdata/hg002/fetch.sh "$data"
pixi run --locked --no-install bash testdata/hg002/preprocess.sh "$data"
pixi run --locked --no-install python3 testdata/hg002/subset.py "$data"
```

Preprocessing decompresses and indexes the FASTA as needed, builds a tabix index for the VCF, and
builds a BAI if the BAM is present. Subsetting writes the CDS-only GTF, the CDS-overlap VCF and its
tabix index, and size counts. With the indexed BAM it also writes a position-depth TSV, an annotated
VCF with `INFO/BAM_POS_DEPTH`, and `*.sampled-indels.min5x.vcf.gz` with a tabix index. The sample
contains two insertions and two deletions per size bucket at ≥5× depth at VCF POS, selected by
SHA-256 rank of the allele. Rerunning subsetting refreshes its outputs in the data directory.

After fetching with `--bam`, `cargo ex-hg002-full` opens the full BAM with the sampled-indel VCF.
`cargo ex-hg002-full-gtf` opens the full BAM and hg38 RefSeq GTF with the depth-annotated CDS-overlap
VCF (`*.with-depth.vcf.gz`) from the same data directory.

The hg38 FASTA and NCBI RefSeq gene annotations come from UCSC. The GTF URL can track newer
annotations. The sample was verified using `ncbiRefSeq.2022-10-28`.
[UCSC hg38 downloads](https://hgdownload.soe.ucsc.edu/goldenPath/hg38/bigZips/)

The VCF is NIST Genome in a Bottle's HG002 GRCh38 v5.0q structural-variant benchmark. If sharing a
derived VCF, acknowledge NIST and include a dated description of the CDS overlap, depth annotation,
and sampling steps used. [NIST data license](https://www.nist.gov/open/license),
[Olson et al.](https://doi.org/10.64898/2026.09.23.752440)

The modBAM is from CSIR-CCMB's ONT Methylation Benchmarking Datasets. Include the date you
accessed the dataset with the registry citation. [AWS Open Data Registry](https://registry.opendata.aws/ont_basemod_data/),
[Kulkarni et al. preprint](https://doi.org/10.1101/2024.11.09.622763),
[Kulkarni et al. published paper](https://doi.org/10.1038/s41467-026-75183-6)

## Rebuilding the HG002 example

Fetch with `--bam` and run the [HG002 preprocessing and subsetting steps](#hg002-grch38-example).
Then use the same data directory to rebuild the three files and their indexes in `hg002/demo/`.

```sh
pixi run --locked --no-install bash testdata/hg002/build_demo.sh "$data"
```

The pinned RefSeq annotation dated 2022-10-28 reproduces the sampled VCF rows. The BAM keeps
alignments overlapping the deletion or the insertion anchor.

Add `--multisample` after the data directory to rebuild the four BAMs, VCF and FASTA in
`hg002/multisample/`. This uses the same sampled-indel VCF and BAM and keeps an additional chr2
reference window at its original coordinates.

## Rebuilding the GTF subset

Fetch the pinned RefSeq download with `bash testdata/fetch.sh DATA_DIR gtf`. The subset contains selected
rows for `IL9R`, `BPY2C`, and `DDX11L16`, in that order. This reproduces the decompressed text,
including the edited spans and the malformed start on one exon.

```sh
gzip -dc testdata/large/hs1.ncbiRefSeq.gtf.gz |
    awk 'BEGIN { FS = OFS = "\t" }
        NR == 1 || NR == 2 { tail = tail $0 "\n" }
        NR == 33 { $5 = 62430599; print }
        NR == 36 { $4 = 62422687; print }
        NR == 34 || NR == 37 || NR == 39 { print }
        NR == 44702 { $5 = 25850512; print }
        NR == 44703 || NR == 44705 || NR == 44707 { print }
        NR == 44710 { $4 = "notanumber"; $5 = 62851171; print }
        END { printf "%s", tail }' |
    gzip -n > testdata/gtf/gtf_subset.gtf.gz
```
