//! Headless profiling runner for BAM decompression, local-area summaries, and pileup hot paths.
//!
//! Run with `--profile profiling --features profiling --bin prof_genome` (or `cargo prof-hg002`).
//! Pair any single subcommand with `cargo flamegraph` for clean single-workload stacks.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use apus::profiling::{ProfileConfig, Workload, run};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WorkloadArg {
    All,
    BgzfSingle,
    BgzfChunked,
    LocalAreaCov,
    #[value(name = "local-area-5mc")]
    LocalArea5mc,
    PileupFetch,
    PileupClassify,
    LocalAreaDraw,
}

impl From<WorkloadArg> for Workload {
    fn from(arg: WorkloadArg) -> Self {
        match arg {
            WorkloadArg::All => Self::All,
            WorkloadArg::BgzfSingle => Self::BgzfSingle,
            WorkloadArg::BgzfChunked => Self::BgzfChunked,
            WorkloadArg::LocalAreaCov => Self::LocalAreaCov,
            WorkloadArg::LocalArea5mc => Self::LocalArea5mc,
            WorkloadArg::PileupFetch => Self::PileupFetch,
            WorkloadArg::PileupClassify => Self::PileupClassify,
            WorkloadArg::LocalAreaDraw => Self::LocalAreaDraw,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Table,
    Tsv,
}

#[derive(Debug, Parser)]
#[command(about = "Profile BAM decompression, local-area summary, and pileup hot paths")]
struct Args {
    /// Indexed BAM file (`.bai` must sit alongside it).
    #[arg(long)]
    bam: PathBuf,
    /// Optional indexed FASTA file (`.fai` alongside it) for 5mC and mismatch workloads.
    #[arg(long)]
    fasta: Option<PathBuf>,
    /// Contig name.
    #[arg(long, default_value = "chr1")]
    contig: String,
    /// 1-based centre genomic position.
    #[arg(long, default_value_t = 108_190_703)]
    pos: u64,
    /// Local-area genomic span in bp (must fit the 576 KB reference budget for 5mC mode).
    #[arg(long, default_value_t = 500_000)]
    span: u64,
    /// Minimum distinct reference CpGs per bin (`:set localareamin5mc`).
    #[arg(long, default_value_t = 10)]
    min_cpgs: u64,
    /// Genomic span in bp for `pileup-fetch`, rounded outward to the BAM window grid.
    #[arg(long, default_value_t = 16_000)]
    pileup_span: u64,
    /// Number of frames per timed iteration for render/classification workloads.
    #[arg(long, default_value_t = 100)]
    frames: usize,
    /// Terminal column width for render/classification workloads.
    #[arg(long, default_value_t = 200)]
    width: u16,
    /// Untimed warmup iterations (warms OS page cache).
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    /// Timed iterations per workload.
    #[arg(long, default_value_t = 3)]
    iters: usize,
    /// Workload to execute.
    #[arg(long, value_enum, default_value_t = WorkloadArg::All)]
    workload: WorkloadArg,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let config = ProfileConfig {
        bam: args.bam,
        fasta: args.fasta,
        contig: args.contig,
        pos_1based: args.pos,
        span: args.span,
        min_cpgs: args.min_cpgs,
        pileup_span: args.pileup_span,
        frames: args.frames,
        width: args.width,
        warmup: args.warmup,
        iters: args.iters,
    };

    match run(&config, args.workload.into()) {
        Ok(report) => {
            match args.format {
                OutputFormat::Table => {
                    println!(
                        "# {}:{}-{} ({} bins) | BAI+header {:.1} ms",
                        report.contig, report.span_start, report.span_end, report.bin_count, report.index_ms
                    );
                    println!();
                    println!("| Workload | Median (ms) | Min (ms) | Max (ms) | Detail |");
                    println!("|---|---:|---:|---:|---|");
                    for row in &report.results {
                        println!(
                            "| `{}` | {:.2} | {:.2} | {:.2} | {} |",
                            row.name, row.median_ms, row.min_ms, row.max_ms, row.detail
                        );
                    }
                }
                OutputFormat::Tsv => {
                    println!("workload\tmedian_ms\tmin_ms\tmax_ms\tdetail");
                    for row in &report.results {
                        println!(
                            "{}\t{:.3}\t{:.3}\t{:.3}\t{}",
                            row.name, row.median_ms, row.min_ms, row.max_ms, row.detail
                        );
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("prof_genome error: {error}");
            ExitCode::FAILURE
        }
    }
}
