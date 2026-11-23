use crate::app::App;
use crate::notification::Severity;
use crate::refgenome::{IndexedFasta, resident::read_windows};
use crate::style::palette::file::FALLBACK;
use crate::viewer_app::{genome::GenomeViewer, vcf::VCFViewer};

/// Builds a single-threaded app from a BGZF VCF, BAM, chromosome FAI, and gzip FASTA slices
/// whose names are `contig:start-end` in 1-based inclusive coordinates.
/// The BAM must contain all alignments overlapping those slices for the Local Area depth.
pub fn build_demo_app(vcf: &[u8], bam: &[u8], fai: &str, fasta: &[u8]) -> Result<App, String> {
    let genome = IndexedFasta::new("hg38".to_string(), fai).map_err(|err| format!("Reference index: {err}"))?;
    let windows = read_windows(fasta, &genome.index).map_err(|err| format!("Reference slices: {err}"))?;
    let genome = GenomeViewer::resident(genome, windows, bam).map_err(|err| format!("BAM: {err}"))?;
    let vcf = VCFViewer::resident(vcf).map_err(|err| format!("VCF: {err}"))?;
    let mut app = App::with_worker_health(vec![Box::new(genome), Box::new(vcf)], None);
    let report = app.run_command(&format!(":set colorscheme \"{FALLBACK}\"\n:focustab VCFViewer"));
    app.report_dispatch(report, Severity::Error);
    Ok(app)
}
