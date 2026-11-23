//! A diagnostic binary that converts a BAM stream to SAM text.
//!
//! It preserves the BAM header and writes every alignment record to stdout, making generated
//! fixtures inspectable with ordinary text tools. This standalone utility crosses the noodles
//! BAM/SAM boundary for portable structural comparison, including on systems without `samtools`.
//! The input path is positional; file and record errors are returned to the process.

use std::io;

use noodles::bam;
use noodles::sam::alignment::io::Write as _;

fn main() -> io::Result<()> {
    let bam_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| panic!("usage: bam_to_sam <bam_path>"));
    let mut reader = bam::io::Reader::new(std::fs::File::open(&bam_path)?);
    let header = reader.read_header()?;
    let stdout = io::stdout();
    let mut writer = noodles::sam::io::Writer::new(stdout.lock());
    writer.write_header(&header)?;
    for result in reader.records() {
        writer.write_alignment_record(&header, &result?)?;
    }
    Ok(())
}
