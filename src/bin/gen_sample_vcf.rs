//! Regenerates `testdata/vcf/sample.vcf.gz` and its `.tbi` from the committed
//! plain-text `testdata/vcf/sample.vcf`. Run from the repository root.
//!
//! The plain `.vcf` is the source of truth and is edited by hand; this binary
//! only produces the bgzf-compressed and indexed siblings, which the fast path
//! and the indexed query are tested against respectively. All three are
//! committed, so a fresh clone never has to run this.

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::Path;

use noodles::bgzf;
use noodles::tabix;
use noodles::vcf;

const SOURCE: &str = "testdata/vcf/sample.vcf";
const COMPRESSED: &str = "testdata/vcf/sample.vcf.gz";

fn main() -> io::Result<()> {
    let mut source = BufReader::new(File::open(SOURCE)?);
    let mut writer = bgzf::io::Writer::new(File::create(COMPRESSED)?);
    io::copy(&mut source, &mut writer)?;
    writer.try_finish()?;

    let index = vcf::fs::index(COMPRESSED)?;
    let index_path = format!("{COMPRESSED}.tbi");
    let mut index_writer = tabix::io::Writer::new(File::create(&index_path)?);
    index_writer.write_index(&index)?;
    index_writer.get_mut().flush()?;

    println!("wrote {COMPRESSED} and {index_path}");
    assert!(Path::new(&index_path).is_file());
    Ok(())
}
