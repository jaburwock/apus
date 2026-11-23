//! Headless profiling workloads for the BAM, local-area, and pileup hot paths.
//!
//! Lives inside `viewer_app::genome` behind the `profiling` feature so it can drive internal
//! production functions directly without widening `src/lib.rs` or affecting normal builds.

use std::collections::HashSet;
use std::fs::File;
use std::hint::black_box;
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use noodles::bam;
use noodles::core::{Position, Region};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::basepair::BasePair;
use crate::refgenome::{IndexedFasta, ReferenceWorkerHandle};
use crate::viewer_app::viewport::ViewPosition;

use super::alignment::{LaneCoverage, fetch_window, sample_window_sizing, windows_touching};
use super::geometry::AreaSpan;
use super::local_area::{
    Breakpoints, LOCAL_AREA_TRACK_HEIGHT, ProfileGridKey, ProfileSummaryCache, SummaryBin, column_span,
    draw_area_chart, draw_area_chart_5mc, scan_summary,
};
use super::reference_data::{ColumnBases, ReferenceCounts, ReferenceData, ReferenceLookup};
use super::track::Contig;
use crate::viewer_app::bam_index::SharedIndex;

const CLASSIFY_WINDOW_LEN: BasePair = BasePair(2_000);
const VISIBLE_LANES: usize = 40;

/// Which profiling workload to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    All,
    BgzfSingle,
    BgzfChunked,
    LocalAreaCov,
    LocalArea5mc,
    PileupFetch,
    PileupClassify,
    LocalAreaDraw,
}

/// Parameters for one profiling run.
#[derive(Debug, Clone)]
pub struct ProfileConfig {
    pub bam: PathBuf,
    pub fasta: Option<PathBuf>,
    pub contig: String,
    /// 1-based genomic centre position, converted at the boundary (invariant 1).
    pub pos_1based: u64,
    pub span: u64,
    pub min_cpgs: u64,
    pub pileup_span: u64,
    pub frames: usize,
    pub width: u16,
    pub warmup: usize,
    pub iters: usize,
}

/// Aggregated timing and diagnostic detail for one workload row.
#[derive(Debug, Clone)]
pub struct WorkloadResult {
    pub name: &'static str,
    pub median_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub detail: String,
}

/// Setup timings plus all workload measurements for a run.
#[derive(Debug, Clone)]
pub struct ProfileReport {
    pub index_ms: f64,
    pub contig: String,
    pub span_start: u64,
    pub span_end: u64,
    pub bin_count: usize,
    pub results: Vec<WorkloadResult>,
}

struct PreparedContext {
    bam: PathBuf,
    index: SharedIndex,
    header: noodles::sam::Header,
    contig: Contig,
    centre: BasePair,
    span: AreaSpan,
    grid_key: ProfileGridKey,
    reference: Option<ReferenceData>,
}

/// Runs the selected workload(s) against the configured BAM and optional FASTA.
pub fn run(config: &ProfileConfig, workload: Workload) -> io::Result<ProfileReport> {
    let t0 = Instant::now();
    let index = SharedIndex::read(&config.bam)?;
    let mut reader = bam::io::Reader::new(File::open(&config.bam)?);
    let header = reader.read_header()?;
    let index_ms = t0.elapsed().as_secs_f64() * 1_000.0;

    let (contig_length, reference) = load_reference(config.fasta.as_deref(), &header, &config.contig)?;
    let contig = Contig {
        name: config.contig.clone(),
        length: contig_length,
    };
    let centre = BasePair::from_1based(config.pos_1based.max(1))
        .min(contig_length.checked_sub(BasePair(1)).unwrap_or(BasePair(0)));
    let view = ViewPosition::new(centre, BasePair::length(config.span.max(1)));
    let span = AreaSpan::for_viewport(view, 1, contig_length);
    let grid_key = ProfileGridKey::for_span(&config.contig, span);
    let bin_count = grid_key.bin_count();

    let mut ctx = PreparedContext {
        bam: config.bam.clone(),
        index,
        header,
        contig,
        centre,
        span,
        grid_key,
        reference,
    };

    let mut results = Vec::new();
    match workload {
        Workload::All => {
            results.push(run_bgzf_single(&ctx, config)?);
            results.push(run_bgzf_chunked(&ctx, config)?);
            let (cov_row, cov_bins) = run_local_area_cov(&ctx, config)?;
            results.push(cov_row);
            let draw_seed = if ctx.reference.is_some() {
                let (row, bins) = run_local_area_5mc(&mut ctx, config)?;
                results.push(row);
                Some((bins, true))
            } else {
                Some((cov_bins, false))
            };
            results.push(run_pileup_fetch(&ctx, config, sample_window_sizing(&mut reader).1)?);
            results.extend(run_pileup_classify(&mut ctx, config)?);
            results.extend(run_local_area_draw(&mut ctx, config, draw_seed)?);
        }
        Workload::BgzfSingle => results.push(run_bgzf_single(&ctx, config)?),
        Workload::BgzfChunked => results.push(run_bgzf_chunked(&ctx, config)?),
        Workload::LocalAreaCov => results.push(run_local_area_cov(&ctx, config)?.0),
        Workload::LocalArea5mc => {
            if ctx.reference.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local-area-5mc requires --fasta",
                ));
            }
            results.push(run_local_area_5mc(&mut ctx, config)?.0);
        }
        Workload::PileupFetch => results.push(run_pileup_fetch(&ctx, config, sample_window_sizing(&mut reader).1)?),
        Workload::PileupClassify => results.extend(run_pileup_classify(&mut ctx, config)?),
        Workload::LocalAreaDraw => results.extend(run_local_area_draw(&mut ctx, config, None)?),
    }

    Ok(ProfileReport {
        index_ms,
        contig: config.contig.clone(),
        span_start: ctx.grid_key.bin_start(0).to_display(),
        span_end: ctx.grid_key.bin_start(bin_count).as_u64(),
        bin_count,
        results,
    })
}

fn load_reference(
    fasta: Option<&Path>,
    header: &noodles::sam::Header,
    contig: &str,
) -> io::Result<(BasePair, Option<ReferenceData>)> {
    if let Some(path) = fasta {
        let genome = IndexedFasta::from_file(path)?;
        let length = genome.index.length(contig).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("contig `{contig}` not found in FASTA index"),
            )
        })?;
        let data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        return Ok((length, Some(data)));
    }
    let length = header
        .reference_sequences()
        .get(contig.as_bytes())
        .map(|seq| BasePair::length(usize::from(seq.length()) as u64))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("contig `{contig}` not found in BAM header"),
            )
        })?;
    Ok((length, None))
}

fn warm_reference_span(
    reference: &mut ReferenceData,
    contig: &Contig,
    start: BasePair,
    end: BasePair,
) -> io::Result<()> {
    let clamped_end = end.min(contig.length);
    if start >= clamped_end {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        reference.poll();
        if let Some(error) = reference.take_error() {
            return Err(io::Error::other(error));
        }
        let mut lookup = ReferenceLookup::new(reference, contig);
        let state = lookup.column_bases(start, clamped_end, ReferenceCounts::BasesAndCpg);
        match state {
            ColumnBases::Complete(_) | ColumnBases::OutOfContig | ColumnBases::NoReference => return Ok(()),
            ColumnBases::Failed => {
                return Err(io::Error::other("failed to load reference chunks for profiling span"));
            }
            ColumnBases::Partial => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out loading reference chunks",
                    ));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn time_workload_with_output<T>(
    name: &'static str,
    warmup: usize,
    iters: usize,
    mut body: impl FnMut() -> io::Result<T>,
    describe: impl FnOnce(&T, f64) -> String,
) -> io::Result<(WorkloadResult, T)> {
    let iters = iters.max(1);
    for _ in 0..warmup {
        let _ = black_box(body()?);
    }
    let mut samples = Vec::with_capacity(iters);
    let mut last = None;
    for _ in 0..iters {
        let start = Instant::now();
        let value = body()?;
        samples.push(start.elapsed().as_secs_f64() * 1_000.0);
        last = Some(value);
    }
    samples.sort_by(f64::total_cmp);
    let median_ms = samples[samples.len() / 2];
    let min_ms = samples[0];
    let max_ms = *samples.last().unwrap_or(&median_ms);
    let last = last.expect("at least one iteration");
    let detail = describe(&last, median_ms);
    Ok((
        WorkloadResult {
            name,
            median_ms,
            min_ms,
            max_ms,
            detail,
        },
        last,
    ))
}

fn time_workload<T>(
    name: &'static str,
    warmup: usize,
    iters: usize,
    body: impl FnMut() -> io::Result<T>,
    describe: impl FnOnce(&T, f64) -> String,
) -> io::Result<WorkloadResult> {
    time_workload_with_output(name, warmup, iters, body, describe).map(|(result, _)| result)
}

fn make_region(contig: &str, start: BasePair, end: BasePair) -> io::Result<Region> {
    let start =
        Position::try_from(usize::try_from(start.as_u64() + 1).map_err(io::Error::other)?).map_err(io::Error::other)?;
    let end = Position::try_from(usize::try_from(end.as_u64()).map_err(io::Error::other)?).map_err(io::Error::other)?;
    Ok(Region::new(contig, start..=end))
}

fn run_bgzf_single(ctx: &PreparedContext, config: &ProfileConfig) -> io::Result<WorkloadResult> {
    let start = ctx.grid_key.bin_start(0);
    let end = ctx.grid_key.bin_start(ctx.grid_key.bin_count());
    let region = make_region(&ctx.contig.name, start, end)?;
    time_workload(
        "bgzf-floor-single",
        config.warmup,
        config.iters,
        || {
            let mut reader = bam::io::IndexedReader::new(File::open(&ctx.bam)?, ctx.index.clone());
            let mut query = reader.query(&ctx.header, &region)?;
            let mut record = bam::Record::default();
            let mut records = 0u64;
            let mut bytes = 0u64;
            loop {
                let n = query.read_record(&mut record)?;
                if n == 0 {
                    break;
                }
                records += 1;
                bytes += n as u64;
            }
            Ok((records, bytes))
        },
        |&(records, bytes), median_ms| {
            let mb = bytes as f64 / (1024.0 * 1024.0);
            let mb_s = if median_ms > 0.0 {
                mb / (median_ms / 1_000.0)
            } else {
                0.0
            };
            format!("{records} recs, {mb:.1} MiB ({mb_s:.0} MiB/s)")
        },
    )
}

fn run_bgzf_chunked(ctx: &PreparedContext, config: &ProfileConfig) -> io::Result<WorkloadResult> {
    // Keep the original query size as a baseline for repeated BGZF decompression.
    const BASELINE_BINS_PER_QUERY: usize = 32;
    let bin_count = ctx.grid_key.bin_count();
    let regions: Vec<Region> = (0..bin_count)
        .step_by(BASELINE_BINS_PER_QUERY)
        .map(|first| {
            make_region(
                &ctx.contig.name,
                ctx.grid_key.bin_start(first),
                ctx.grid_key.bin_start((first + BASELINE_BINS_PER_QUERY).min(bin_count)),
            )
        })
        .collect::<io::Result<_>>()?;
    time_workload(
        "bgzf-floor-chunked",
        config.warmup,
        config.iters,
        || {
            let mut record = bam::Record::default();
            let mut records = 0u64;
            let mut bytes = 0u64;
            for region in &regions {
                let mut reader = bam::io::IndexedReader::new(File::open(&ctx.bam)?, ctx.index.clone());
                let mut query = reader.query(&ctx.header, region)?;
                loop {
                    let n = query.read_record(&mut record)?;
                    if n == 0 {
                        break;
                    }
                    records += 1;
                    bytes += n as u64;
                }
            }
            Ok((records, bytes))
        },
        |&(records, bytes), median_ms| {
            let mb = bytes as f64 / (1024.0 * 1024.0);
            let mb_s = if median_ms > 0.0 {
                mb / (median_ms / 1_000.0)
            } else {
                0.0
            };
            format!(
                "{} chunks, {records} recs, {mb:.1} MiB ({mb_s:.0} MiB/s)",
                regions.len()
            )
        },
    )
}

fn fetch_summary_bins(ctx: &PreparedContext, qualifying: Option<&[bool]>) -> io::Result<Vec<SummaryBin>> {
    let mut bins = Vec::with_capacity(ctx.grid_key.bin_count());
    scan_summary(
        &ctx.bam,
        &ctx.index,
        &ctx.header,
        &ctx.grid_key,
        qualifying.filter(|flags| flags.iter().any(|&flag| flag)),
        || false,
        |batch| bins.extend(batch.bins),
    )?;
    Ok(bins)
}

fn run_local_area_cov(ctx: &PreparedContext, config: &ProfileConfig) -> io::Result<(WorkloadResult, Vec<SummaryBin>)> {
    time_workload_with_output(
        "local-area-cov",
        config.warmup,
        config.iters,
        || fetch_summary_bins(ctx, None),
        |bins, _| {
            let mean_depth = bins.iter().map(|b| b.spanning_depth as f64).sum::<f64>() / bins.len().max(1) as f64;
            let indel_bins = bins
                .iter()
                .filter(|b| b.deletion_depth > 0 || b.insertion_depth > 0)
                .count();
            format!("{} bins, mean {mean_depth:.1}x, {indel_bins} indel bins", bins.len())
        },
    )
}

fn run_local_area_5mc(
    ctx: &mut PreparedContext,
    config: &ProfileConfig,
) -> io::Result<(WorkloadResult, Vec<SummaryBin>)> {
    let reference = ctx.reference.as_mut().expect("caller checks reference");
    let grid_start = ctx.grid_key.bin_start(0);
    let grid_end = ctx.grid_key.bin_start(ctx.grid_key.bin_count());
    let fits = ReferenceLookup::new(reference, &ctx.contig).reference_band_fits(
        grid_start,
        grid_end.min(ctx.contig.length),
        ReferenceCounts::BasesAndCpg,
    );
    if !fits {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "span {}..{} exceeds the 18-chunk (576 KB) reference band budget; pass a smaller --span",
                grid_start.as_u64(),
                grid_end.as_u64()
            ),
        ));
    }
    warm_reference_span(reference, &ctx.contig, grid_start, grid_end)?;

    let mut qualifying = Vec::with_capacity(ctx.grid_key.bin_count());
    for index in 0..ctx.grid_key.bin_count() {
        let start = ctx.grid_key.bin_start(index);
        let end = ctx.grid_key.bin_start(index + 1).min(ctx.contig.length);
        let mut lookup = ReferenceLookup::new(reference, &ctx.contig);
        let eligible = matches!(
            lookup.column_bases(start, end, ReferenceCounts::BasesAndCpg),
            ColumnBases::Complete(hist) if u64::from(hist.cpg) >= config.min_cpgs
        );
        qualifying.push(eligible);
    }
    let qualifying_bins = qualifying.iter().filter(|&&q| q).count();

    time_workload_with_output(
        "local-area-5mc",
        config.warmup,
        config.iters,
        || fetch_summary_bins(ctx, Some(&qualifying)),
        |bins, _| {
            let total_calls: u64 = bins.iter().map(|b| b.mod_5mc_call_count).sum();
            format!(
                "{qualifying_bins}/{} bins qualify (min_cpgs={}), {total_calls} 5mC calls",
                bins.len(),
                config.min_cpgs
            )
        },
    )
}

#[derive(Default)]
struct PileupFetchStats {
    read_copies: usize,
    unique_reads: usize,
    in_span: usize,
    seq_bytes: usize,
    cigar_bytes: usize,
    mod_bytes: usize,
    read_bytes: usize,
    counts_bytes: usize,
}

fn run_pileup_fetch(ctx: &PreparedContext, config: &ProfileConfig, window_len: BasePair) -> io::Result<WorkloadResult> {
    let view = ViewPosition::new(ctx.centre, BasePair::length(config.pileup_span));
    let span = AreaSpan::for_viewport(view, 1, ctx.contig.length);
    let windows = windows_touching(span.start(), span.end(), window_len);

    time_workload(
        "pileup-fetch",
        config.warmup,
        config.iters,
        || {
            let mut stats = PileupFetchStats::default();
            let mut seen = HashSet::new();
            for &start in &windows {
                let end = start + window_len;
                let data = fetch_window(&ctx.bam, &ctx.index, &ctx.header, &ctx.contig.name, start, end, || {
                    false
                })?;
                stats.read_copies += data.reads.len();
                stats.read_bytes += data.reads.capacity() * size_of::<super::alignment::Read>();
                stats.counts_bytes += data.counts.bases.capacity() * size_of::<super::alignment::BaseCounts>()
                    + data.counts.calls.capacity() * size_of::<super::alignment::CallCounts>();
                for read in &data.reads {
                    if seen.insert((read.id, read.start))
                        && read.start < span.end()
                        && read.start + read.length > span.start()
                    {
                        stats.in_span += 1;
                    }
                    if let Some(seq) = &read.seq {
                        stats.seq_bytes += seq.byte_len();
                    }
                    stats.cigar_bytes += std::mem::size_of_val(read.cigar.ops());
                    if let Some(mods) = &read.mod_5mc {
                        stats.mod_bytes += mods.calls.capacity() * size_of::<(u32, u8)>();
                    }
                }
            }
            stats.unique_reads = seen.len();
            Ok(stats)
        },
        |stats, _| {
            let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
            let total = stats.seq_bytes + stats.cigar_bytes + stats.mod_bytes + stats.read_bytes + stats.counts_bytes;
            format!(
                "{} × {} bp windows for {} bp, {} unique reads ({} in span, {} copies), heap {:.2} MiB (seq {:.2}, cigar {:.2}, 5mC {:.2}, reads {:.2}, counts {:.2})",
                windows.len(),
                window_len.as_u64(),
                span.width().as_u64(),
                stats.unique_reads,
                stats.in_span,
                stats.read_copies,
                mib(total),
                mib(stats.seq_bytes),
                mib(stats.cigar_bytes),
                mib(stats.mod_bytes),
                mib(stats.read_bytes),
                mib(stats.counts_bytes),
            )
        },
    )
}

fn run_pileup_classify(ctx: &mut PreparedContext, config: &ProfileConfig) -> io::Result<Vec<WorkloadResult>> {
    let win_start = ctx.centre - (ctx.centre % CLASSIFY_WINDOW_LEN);
    let win_end = (win_start + CLASSIFY_WINDOW_LEN).min(ctx.contig.length);
    let data = fetch_window(
        &ctx.bam,
        &ctx.index,
        &ctx.header,
        &ctx.contig.name,
        win_start,
        win_end,
        || false,
    )?;
    let reads = &data.reads[..data.reads.len().min(VISIBLE_LANES)];
    let frames = config.frames.max(1);

    let mut fallback_ref = if ctx.reference.is_none() {
        Some(ReferenceData::new(
            &IndexedFasta::hs1_fallback(),
            ReferenceWorkerHandle::spawn(),
        ))
    } else {
        None
    };
    let reference = ctx
        .reference
        .as_mut()
        .or(fallback_ref.as_mut())
        .expect("reference exists");

    let left_close = ctx
        .centre
        .checked_sub(BasePair(u64::from(config.width) / 2))
        .unwrap_or(BasePair(0));
    let left_mid = ctx
        .centre
        .checked_sub(BasePair(u64::from(config.width) * 5))
        .unwrap_or(BasePair(0));
    warm_reference_span(
        reference,
        &ctx.contig,
        left_mid,
        left_mid + BasePair(u64::from(config.width) * 10),
    )?;

    let cases = [
        ("pileup-classify-close", left_close, 1u64, false),
        ("pileup-classify-mid", left_mid, 10u64, false),
        ("pileup-classify-mid-5mc", left_mid, 10u64, true),
    ];

    let mut out = Vec::with_capacity(cases.len());
    for (name, left, bp_per_col, collect_5mc) in cases {
        let mut coverage = LaneCoverage::for_paint(config.width, left, bp_per_col, collect_5mc);
        let result = time_workload(
            name,
            config.warmup,
            config.iters,
            || {
                let mut checksum = 0usize;
                for _ in 0..frames {
                    for read in reads {
                        coverage.reset(config.width, left, bp_per_col);
                        coverage.classify_read_against(
                            read,
                            config.width,
                            ReferenceLookup::new(reference, &ctx.contig),
                        );
                        checksum = checksum.wrapping_add(usize::from(coverage.past_last_col));
                    }
                }
                Ok(checksum)
            },
            |_, median_ms| {
                let per_frame = median_ms / frames as f64;
                format!(
                    "{frames} frames x {} reads @ {} cols ({per_frame:.3} ms/frame)",
                    reads.len(),
                    config.width
                )
            },
        )?;
        out.push(result);
    }
    Ok(out)
}

fn run_local_area_draw(
    ctx: &mut PreparedContext,
    config: &ProfileConfig,
    seeded: Option<(Vec<SummaryBin>, bool)>,
) -> io::Result<Vec<WorkloadResult>> {
    let frames = config.frames.max(1);
    let area = Rect::new(0, 0, config.width, LOCAL_AREA_TRACK_HEIGHT);
    let mut buffer = Buffer::empty(area);

    // Populate one real bin vector once so draw benchmarks measure rendering, not BAM I/O.
    // `Workload::All` passes bins already produced by `local-area-5mc` / `local-area-cov`.
    let has_ref = ctx.reference.is_some();
    let (bins, ref_band_ready) = if let Some((bins, ref_ready)) = seeded {
        (bins, ref_ready)
    } else {
        let qualifying = if let Some(reference) = ctx.reference.as_mut() {
            let grid_start = ctx.grid_key.bin_start(0);
            let grid_end = ctx.grid_key.bin_start(ctx.grid_key.bin_count());
            let fits = ReferenceLookup::new(reference, &ctx.contig).reference_band_fits(
                grid_start,
                grid_end.min(ctx.contig.length),
                ReferenceCounts::BasesAndCpg,
            );
            if fits {
                warm_reference_span(reference, &ctx.contig, grid_start, grid_end)?;
                let mut q = Vec::with_capacity(ctx.grid_key.bin_count());
                for index in 0..ctx.grid_key.bin_count() {
                    let start = ctx.grid_key.bin_start(index);
                    let end = ctx.grid_key.bin_start(index + 1).min(ctx.contig.length);
                    let mut lookup = ReferenceLookup::new(reference, &ctx.contig);
                    q.push(matches!(
                        lookup.column_bases(start, end, ReferenceCounts::BasesAndCpg),
                        ColumnBases::Complete(hist) if u64::from(hist.cpg) >= config.min_cpgs
                    ));
                }
                Some(q)
            } else {
                None
            }
        } else {
            None
        };

        (fetch_summary_bins(ctx, qualifying.as_deref())?, qualifying.is_some())
    };

    let mut cache = ProfileSummaryCache::new();
    cache.seed_bins(&ctx.contig.name, ctx.span, bins);

    let mut out = Vec::new();
    let breakpoints = Breakpoints {
        enabled: true,
        angle: 12.0,
    };
    out.push(time_workload(
        "local-area-draw-cov",
        config.warmup,
        config.iters,
        || {
            for _ in 0..frames {
                draw_area_chart(&mut buffer, area, ctx.span, &cache, breakpoints, &[]);
                black_box(buffer.content.first());
            }
            Ok(())
        },
        |_, median_ms| {
            format!(
                "{frames} frames @ {}x{} ({:.3} ms/frame)",
                area.width,
                area.height,
                median_ms / frames as f64
            )
        },
    )?);

    out.push(time_workload(
        "local-area-draw-5mc",
        config.warmup,
        config.iters,
        || {
            for _ in 0..frames {
                draw_area_chart_5mc(&mut buffer, area, ctx.span, &cache);
                black_box(buffer.content.first());
            }
            Ok(())
        },
        |_, median_ms| {
            format!(
                "{frames} frames @ {}x{} ({:.3} ms/frame)",
                area.width,
                area.height,
                median_ms / frames as f64
            )
        },
    )?);

    if has_ref && ref_band_ready {
        let reference = ctx.reference.as_mut().expect("checked");
        out.push(time_workload(
            "ref-cpg-band-draw",
            config.warmup,
            config.iters,
            || {
                let mut total_cpgs = 0u64;
                for _ in 0..frames {
                    let mut lookup = ReferenceLookup::new(reference, &ctx.contig);
                    for col in 0..config.width {
                        let (start, end) = column_span(ctx.span, col, config.width);
                        if let ColumnBases::Complete(hist) =
                            lookup.column_bases(start, end, ReferenceCounts::BasesAndCpg)
                        {
                            total_cpgs += u64::from(hist.cpg);
                        }
                    }
                }
                Ok(total_cpgs)
            },
            |&total_cpgs, median_ms| {
                format!(
                    "{frames} frames x {} cols, {} cpgs/frame ({:.3} ms/frame)",
                    config.width,
                    total_cpgs / frames as u64,
                    median_ms / frames as f64
                )
            },
        )?);
    }

    Ok(out)
}
