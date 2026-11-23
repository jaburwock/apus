#!/usr/bin/env python3
"""Build the hg38 CDS-overlap VCF and optionally sample indels using ONT depth."""

import argparse
import gzip
import hashlib
import re
import subprocess
from bisect import bisect_left
from collections import Counter
from pathlib import Path
from tempfile import TemporaryDirectory


RELEASE = "HG002_GRCh38_v5.0q_stvar"
STEM = f"{RELEASE}.ncbiRefSeq-cds-overlap"
UPPER_BOUNDS = (19, 49, 99, 499, 999, 4999, 9999)
BUCKETS = ("1-19", "20-49", "50-99", "100-499", "500-999", "1000-4999", "5000-9999", ">=10000")
BASES = re.compile(r"[ACGTNacgtn]+")
DEPTH_TAG = "BAM_POS_DEPTH"
MIN_DEPTH = 5


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def capture(*args):
    return run(*args, stdout=subprocess.PIPE, text=True).stdout


def cds_overlap(gtf, vcf, stage):
    cds = stage / "hg38.ncbiRefSeq.cds.gtf.gz"
    intervals = stage / "cds.tsv"
    with gzip.open(gtf, "rb") as features, cds.open("wb") as archive, intervals.open("wb") as spans:
        with gzip.GzipFile(filename="", fileobj=archive, mode="wb", mtime=0) as coding:
            for line in features:
                if line.startswith(b"#"):
                    coding.write(line)
                    continue
                fields = line.split(b"\t")
                if fields[2] == b"CDS":
                    chrom, start, end = fields[0], fields[3], fields[4]
                    if not 1 <= int(start) <= int(end):
                        raise ValueError(f"Invalid CDS coordinates: {line!r}")
                    coding.write(line)
                    # GTF and the bcftools TSV are both 1-based and inclusive.
                    spans.write(b"\t".join((chrom, start, end)) + b"\n")
    if not intervals.stat().st_size:
        raise ValueError(f"No CDS rows in {gtf}")
    overlap = stage / f"{STEM}.vcf.gz"
    run("bcftools", "view", "--no-version", "--targets-file", intervals,
        "--targets-overlap", "variant", "-Oz", "--write-index=tbi", "-o", overlap, vcf)
    return cds, overlap


def variant_group(ref, alt):
    if alt == "*":
        return "spanning_deletion_placeholder", "."
    if not (BASES.fullmatch(ref) and BASES.fullmatch(alt)):
        raise ValueError(f"Unsupported alleles: {ref!r}, {alt!r}")
    change = len(alt) - len(ref)
    if not change:
        return ("SNV", "1") if len(ref) == 1 else ("equal_length_replacement", ".")
    return ("INS" if change > 0 else "DEL"), BUCKETS[bisect_left(UPPER_BOUNDS, abs(change))]


def write_size_counts(vcf, output):
    records, tagged = Counter(), Counter()
    with gzip.open(vcf, "rt") as source:
        for line in source:
            if line.startswith("#"):
                continue
            _, _, _, ref, alt, _, _, info, *_ = line.split("\t")
            group = variant_group(ref, alt)
            records[group] += 1
            tagged[group] += any(tag.startswith("SVTYPE=") and tag != "SVTYPE=." for tag in info.split(";"))

    with output.open("w") as out:
        print("kind", "size_bp", "insertions", "deletions", "records", "sv_tagged_records", sep="\t", file=out)
        print("SNV", 1, 0, 0, records["SNV", "1"], tagged["SNV", "1"], sep="\t", file=out)
        replacement = ("equal_length_replacement", ".")
        if records[replacement]:
            print(*replacement, 0, 0, records[replacement], tagged[replacement], sep="\t", file=out)
        for bucket in BUCKETS:
            ins, dele = records["INS", bucket], records["DEL", bucket]
            print("indel", bucket, ins, dele, ins + dele, tagged["INS", bucket] + tagged["DEL", bucket],
                  sep="\t", file=out)
        placeholder = ("spanning_deletion_placeholder", ".")
        print(*placeholder, 0, 0, records[placeholder], tagged[placeholder], sep="\t", file=out)


def position_depths(positions, bam, stage):
    lengths = {}
    for line in capture("samtools", "view", "-H", bam).splitlines():
        if line.startswith("@SQ\t"):
            tags = dict(field.split(":", 1) for field in line.split("\t")[1:])
            lengths[tags["SN"]] = int(tags["LN"])

    depths = {(chrom, pos): 0 for chrom, pos in positions if 1 <= pos <= lengths.get(chrom, 0)}
    if not depths:
        return depths
    bed = stage / "positions.bed"
    with bed.open("w") as out:
        for chrom, pos in depths:
            print(chrom, pos - 1, pos, sep="\t", file=out)

    # depth -b limits output positions. view -M limits the BAM reads using its index.
    view = ("samtools", "view", "--no-PG", "-u", "-M", "-L", bed, bam)
    with subprocess.Popen(view, stdout=subprocess.PIPE) as reads:
        try:
            result = run("samtools", "depth", "-b", bed, "-", stdin=reads.stdout,
                         stdout=subprocess.PIPE, text=True)
        finally:
            reads.stdout.close()
        if reads.wait():
            raise subprocess.CalledProcessError(reads.returncode, view)
    for line in result.stdout.splitlines():
        chrom, pos, depth = line.split("\t")
        depths[chrom, int(pos)] = int(depth)
    return depths


def annotate_depth(vcf, bam, stage):
    coords = capture("bcftools", "query", "-f", r"%CHROM\t%POS\n", vcf)
    positions = [(chrom, int(pos)) for chrom, pos in (line.split("\t") for line in coords.splitlines())]
    depths = position_depths(positions, bam, stage)
    report = stage / f"{STEM}.pos-depth.tsv"
    with report.open("w") as out:
        print("CHROM", "POS", "DEPTH", sep="\t", file=out)
        for chrom, pos in positions:
            print(chrom, pos, depths.get((chrom, pos), "."), sep="\t", file=out)

    table = stage / "depth.tsv.gz"
    rows = "".join(f"{chrom}\t{pos}\t{depth}\n" for (chrom, pos), depth in depths.items())
    with table.open("wb") as out:
        run("bgzip", "-c", input=rows.encode(), stdout=out)
    run("tabix", "-s1", "-b2", "-e2", table)
    annotated = stage / f"{STEM}.with-depth.vcf.gz"
    header = f'##INFO=<ID={DEPTH_TAG},Number=1,Type=Integer,Description="HG002 ONT BAM depth from samtools depth defaults at the 1-based VCF POS">'
    run("bcftools", "annotate", "--no-version", "-a", table, "-c", f"CHROM,POS,INFO/{DEPTH_TAG}",
        "-H", header, "-Oz", "--write-index=tbi", "-o", annotated, vcf)
    return report, annotated


def sample_indels(vcf, output, depth_threshold):
    headers = []
    groups = {(kind, bucket): {} for bucket in BUCKETS for kind in ("INS", "DEL")}
    prefix = f"{DEPTH_TAG}="
    with gzip.open(vcf, "rt") as source:
        for index, line in enumerate(source):
            if line.startswith("#"):
                headers.append(line)
                continue
            chrom, pos, _, ref, alt, _, _, info, *_ = line.split("\t")
            depth = next((tag[len(prefix):] for tag in info.split(";") if tag.startswith(prefix)), ".")
            if depth == "." or int(depth) < depth_threshold:
                continue
            group = variant_group(ref, alt)
            if group in groups:
                allele = "\t".join((chrom, pos, ref.upper(), alt.upper())).encode()
                groups[group].setdefault(allele, (index, line))

    selected = []
    for (kind, bucket), records in groups.items():
        if len(records) < 2:
            raise ValueError(f"{bucket} bp {kind}: need two distinct alleles at ≥{depth_threshold}×, found {len(records)}")
        # Bytewise SHA-256 ranking is independent of host locale and Python's random seed.
        ranked = sorted(records, key=lambda allele: (hashlib.sha256(allele).digest(), allele))
        selected.extend(records[allele] for allele in ranked[:2])
    body = "".join(line for _, line in sorted(selected))
    run("bcftools", "view", "--no-version", "-Oz", "--write-index=tbi", "-o", output, "-",
        input=("".join(headers) + body).encode())


def with_index(vcf):
    return [vcf, vcf.with_name(f"{vcf.name}.tbi")]


def build(data):
    bam = data / "HG002_public_5kHz_sup_v5.2r2_5mC.bam"
    if bam.is_file() and not bam.with_suffix(".bam.bai").is_file():
        raise ValueError("Run preprocess.sh to index the HG002 BAM")
    with TemporaryDirectory(prefix=".hg002-subset.", dir=data) as temporary:
        stage = Path(temporary)
        cds, overlap = cds_overlap(data / "hg38.ncbiRefSeq.gtf.gz", data / f"{RELEASE}.vcf.gz", stage)
        counts = stage / f"{STEM}.size-counts.tsv"
        write_size_counts(overlap, counts)
        outputs = [cds, counts, *with_index(overlap)]
        if bam.is_file():
            depths, annotated = annotate_depth(overlap, bam, stage)
            sample = stage / f"{STEM}.sampled-indels.min{MIN_DEPTH}x.vcf.gz"
            sample_indels(annotated, sample, MIN_DEPTH)
            outputs.extend([depths, *with_index(annotated), *with_index(sample)])
        else:
            print("Add the indexed HG002 BAM to generate depth annotations and the 5× sample.")
        for output in outputs:
            destination = data / output.name
            output.replace(destination)
            print(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data_dir", type=Path, help="directory containing the HG002 inputs")
    args = parser.parse_args()
    try:
        build(args.data_dir.resolve())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{parser.prog}: {error}\n")


if __name__ == "__main__":
    main()
