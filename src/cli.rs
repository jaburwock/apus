//! Clap-facing command-line input for startup composition.
//!
//! Positional paths remain in argv order. The typed helpers on [`FileArgs`] identify the first
//! file of each supported kind, while [`FileArgs::focus_targets`] preserves every viewer-bearing
//! argument so startup can reproduce the user's ordering when selecting the initial tab.
//!
//! This module only parses and classifies arguments. Startup turns the resulting values into the
//! ordinary command batch routed by `App`; it does not load files or dispatch commands here.

use std::path::{Path, PathBuf};

/// What kind of file a positional argument names, inferred from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Fasta,
    /// A gzipped GTF.
    Gtf,
    Bam,
    Vcf,
    Other,
}

impl EntryKind {
    /// Compressed inputs are told apart by the stem's extension.
    fn from_path(path: &Path) -> Self {
        let ext = path.extension().and_then(|e| e.to_str());
        let stem_ext = path
            .file_stem()
            .map(Path::new)
            .and_then(|s| s.extension())
            .and_then(|e| e.to_str());
        match (ext, stem_ext) {
            (Some("fa" | "fasta"), _) | (Some("gz" | "bgz"), Some("fa" | "fasta")) => EntryKind::Fasta,
            (Some("bam"), _) | (Some("cram"), _) => EntryKind::Bam,
            (Some("gz"), Some("gtf")) => EntryKind::Gtf,
            (Some("vcf"), _) | (Some("gz"), Some("vcf")) => EntryKind::Vcf,
            _ => EntryKind::Other,
        }
    }
}

#[derive(clap::Args, Debug, Clone, Default)]
pub struct FileArgs {
    /// Read startup commands from this file.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    pub files: Vec<PathBuf>,
}

#[derive(clap::Parser)]
#[command(name = "apus")]
pub struct Cli {
    #[command(flatten)]
    pub files: FileArgs,
}

impl FileArgs {
    /// The first trailing FASTA file, if any.
    pub fn fasta_arg(&self) -> Option<&PathBuf> {
        self.files
            .iter()
            .find(|path| EntryKind::from_path(path) == EntryKind::Fasta)
    }

    /// The first trailing `.bam` file, if any.
    pub fn bam_arg(&self) -> Option<&PathBuf> {
        self.bam_args().next()
    }

    /// Alignment paths in argv order.
    pub(crate) fn bam_args(&self) -> impl Iterator<Item = &PathBuf> {
        self.files
            .iter()
            .filter(|path| EntryKind::from_path(path) == EntryKind::Bam)
    }

    /// The first trailing `.vcf`/`.vcf.gz` file, if any.
    pub fn vcf_arg(&self) -> Option<&PathBuf> {
        self.files
            .iter()
            .find(|path| EntryKind::from_path(path) == EntryKind::Vcf)
    }

    /// The first trailing `.gtf.gz` file, if any.
    pub fn gtf_arg(&self) -> Option<&PathBuf> {
        self.files
            .iter()
            .find(|path| EntryKind::from_path(path) == EntryKind::Gtf)
    }

    /// The viewer each named file is shown in, in argv order, so startup can
    /// focus the tab holding the last one. Files with no view are skipped.
    pub fn focus_targets(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.files.iter().filter_map(|path| match EntryKind::from_path(path) {
            EntryKind::Fasta | EntryKind::Bam | EntryKind::Gtf => Some("GenomeViewer"),
            EntryKind::Vcf => Some("VCFViewer"),
            EntryKind::Other => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_flattens_positional_file_args_in_order() {
        let cli = Cli::try_parse_from([
            "apus",
            "reads.bam",
            "--config",
            "startup.txt",
            "genome.fa.gz",
            "variants.vcf.gz",
            "other.bam",
        ])
        .expect("config and positional paths must parse through flattened FileArgs");
        assert_eq!(cli.files.config, Some(PathBuf::from("startup.txt")));
        assert_eq!(
            cli.files.files,
            vec![
                PathBuf::from("reads.bam"),
                PathBuf::from("genome.fa.gz"),
                PathBuf::from("variants.vcf.gz"),
                PathBuf::from("other.bam"),
            ]
        );
        assert_eq!(cli.files.fasta_arg(), Some(&PathBuf::from("genome.fa.gz")));
        assert_eq!(cli.files.bam_arg(), Some(&PathBuf::from("reads.bam")));
        assert_eq!(
            cli.files.bam_args().collect::<Vec<_>>(),
            [&PathBuf::from("reads.bam"), &PathBuf::from("other.bam")]
        );
        assert_eq!(cli.files.vcf_arg(), Some(&PathBuf::from("variants.vcf.gz")));
        assert_eq!(
            cli.files.focus_targets().collect::<Vec<_>>(),
            vec!["GenomeViewer", "GenomeViewer", "VCFViewer", "GenomeViewer"]
        );
    }
}
