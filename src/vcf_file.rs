//! Seekable VCF input and on-demand genotype lookup for variant jumps.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use noodles::{bgzf, vcf, vcf::variant::record::samples::keys::key};

/// Where a record starts, in units specific to the file's compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordOffset {
    Plain(u64),
    Bgzf(u64),
}

/// Reopens one VCF record without retaining genotypes in the row buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlternateSampleLookup {
    pub(crate) path: PathBuf,
    pub(crate) sample_names: Arc<[String]>,
    /// Without a checkpoint, skip records from the end of the header.
    pub(crate) offset: Option<RecordOffset>,
    pub(crate) skip: usize,
}

impl AlternateSampleLookup {
    pub(crate) fn read(&self) -> io::Result<HashSet<&str>> {
        let mut source = Source::open(&self.path)?;
        match self.offset {
            Some(offset) => source.seek(offset)?,
            None => {
                vcf::io::Reader::new(source.as_buf_read()).read_header()?;
            }
        }
        let mut reader = vcf::io::Reader::new(source.as_buf_read());
        let mut record = vcf::Record::default();
        for _ in 0..=self.skip {
            if reader.read_record(&mut record)? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "VCF row was not found"));
            }
        }
        Ok(alternate_sample_indices(&record.samples())
            .filter_map(|index| self.sample_names.get(index).map(String::as_str))
            .collect())
    }
}

/// A partial call still counts if it identifies an alternate allele.
pub(crate) fn alternate_sample_indices<'a>(samples: &'a vcf::record::Samples<'_>) -> impl Iterator<Item = usize> + 'a {
    let gt_index = samples.keys().iter().position(|key| key == key::GENOTYPE);
    gt_index.into_iter().flat_map(move |gt_index| {
        samples.iter().enumerate().filter_map(move |(index, sample)| {
            let gt = sample.as_ref().split(':').nth(gt_index)?;
            gt.split(['/', '|'])
                .any(|allele| allele.parse::<usize>().is_ok_and(|allele| allele > 0))
                .then_some(index)
        })
    })
}

pub(crate) enum Source {
    Plain(BufReader<File>),
    Bgzf(Box<bgzf::io::Reader<File>>),
}

impl Source {
    /// Uses the same extensions as the VCF reader builder, which erases seek support.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        Ok(match path.extension().and_then(|ext| ext.to_str()) {
            Some("gz" | "bgz") => Self::Bgzf(Box::new(bgzf::io::Reader::new(file))),
            _ => Self::Plain(BufReader::new(file)),
        })
    }

    pub(crate) fn as_buf_read(&mut self) -> &mut dyn BufRead {
        match self {
            Self::Plain(reader) => reader,
            Self::Bgzf(reader) => reader.as_mut(),
        }
    }

    pub(crate) fn position(&mut self) -> io::Result<RecordOffset> {
        Ok(match self {
            Self::Plain(reader) => RecordOffset::Plain(reader.stream_position()?),
            Self::Bgzf(reader) => RecordOffset::Bgzf(u64::from(reader.virtual_position())),
        })
    }

    pub(crate) fn seek(&mut self, offset: RecordOffset) -> io::Result<()> {
        match (self, offset) {
            (Self::Plain(reader), RecordOffset::Plain(byte)) => reader.seek(SeekFrom::Start(byte)).map(|_| ()),
            (Self::Bgzf(reader), RecordOffset::Bgzf(position)) => {
                reader.seek(bgzf::VirtualPosition::from(position)).map(|_| ())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "index offset does not match the file's compression",
            )),
        }
    }
}
