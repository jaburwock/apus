//! Regenerates `testdata/vcf/clinvar_subset.vcf.gz` and its `.tbi` from the
//! full ClinVar VCF in `testdata/large/`. Fetch it with `bash testdata/fetch.sh DATA_DIR clinvar`.
//!
//! Unlike `sample.vcf`, this fixture is *derived* rather than hand-edited: the
//! `.gz` is the artifact, and the downloaded VCF plus this binary are its source.
//! Both outputs are committed, so a fresh clone never has to run this — which is
//! the point, since the input is a ~99MB gitignored download.

use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::Path;

use noodles::bgzf;
use noodles::tabix;
use noodles::vcf;

const SOURCE: &str = "testdata/large/clinvar_20240624_GCA_009914755.4.vcf.gz";
const OUTPUT: &str = "testdata/vcf/clinvar_subset.vcf.gz";

/// Keep every `STRIDE`-th record. 57 takes ClinVar's 2,866,168 records down to
/// 50,283 (~2.5MB compressed) while leaving every one of the 24 contigs
/// represented — the sparsest, `Y`, has 96 records in the source and still
/// contributes 2.
///
/// Striding file-wide rather than taking a head keeps each contig a single
/// contiguous, coordinate-sorted block, which is what makes the result validly
/// tabix-indexable.
const STRIDE: usize = 57;

fn main() -> io::Result<()> {
    if !Path::new(SOURCE).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{SOURCE} is missing. Run `bash testdata/fetch.sh DATA_DIR clinvar`."),
        ));
    }

    let mut reader = bgzf::io::Reader::new(File::open(SOURCE)?);
    let mut writer = bgzf::io::Writer::new(File::create(OUTPUT)?);

    // Copying raw lines rather than parsed records keeps ClinVar's INFO text
    // byte-identical, sparing the fixture a re-serialisation round-trip that
    // could normalise away the very shapes it exists to show.
    let mut header_lines = 0usize;
    let mut seen = 0usize;
    let mut kept = 0usize;
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        if line.starts_with('#') {
            writer.write_all(line.as_bytes())?;
            header_lines += 1;
        } else {
            seen += 1;
            if seen.is_multiple_of(STRIDE) {
                writer.write_all(line.as_bytes())?;
                kept += 1;
            }
        }
        line.clear();
    }
    writer.try_finish()?;

    let index = vcf::fs::index(OUTPUT)?;
    let index_path = format!("{OUTPUT}.tbi");
    let mut index_writer = tabix::io::Writer::new(File::create(&index_path)?);
    index_writer.write_index(&index)?;
    index_writer.get_mut().flush()?;

    println!("wrote {OUTPUT} and {index_path}");
    println!("{header_lines} header lines, {kept} of {seen} records kept (every {STRIDE}th)");
    Ok(())
}
