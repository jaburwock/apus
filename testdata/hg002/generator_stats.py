#!/usr/bin/env python3
"""Print aggregate calibration statistics for the synthetic BAMs."""

import argparse
from collections import Counter
import json
import re
from subprocess import PIPE, Popen, check_output
from tempfile import NamedTemporaryFile


CIGAR_OP = re.compile(rb"(\d+)([MIDNSHP=X])")
MD_CHANGE = re.compile(rb"\^[A-Z]+|[A-Z]")
INDEL_BUCKETS = [(1, 1), (2, 2), (3, 3), (4, 5), (6, 10), (11, 20), (21, 50), (51, 100), (101, 300)]
MAPQ = 60


def sample_windows(bam):
    contig_lengths = {}
    for line in check_output(["samtools", "view", "-H", bam]).splitlines():
        if line.startswith(b"@SQ\t"):
            tags = dict(field.split(b":", 1) for field in line.split(b"\t")[1:])
            contig_lengths[tags[b"SN"]] = int(tags[b"LN"])

    windows = {}
    for chromosome in range(1, 23):
        name = f"chr{chromosome}".encode()
        centers = [contig_lengths[name] * decile // 10 for decile in range(1, 10)]
        windows[name] = [(center - 50_000, center + 50_000) for center in centers]
    return windows


def sample_alignments(bam, windows):
    with NamedTemporaryFile() as bed:
        for name, intervals in windows.items():
            for start, end in intervals:
                bed.write(b"%s\t%d\t%d\n" % (name, start, end))
        bed.flush()
        with Popen(
            ["samtools", "view", "-M", "-L", bed.name, "-F", "5", "--keep-tag", "MD", bam],
            stdout=PIPE,
        ) as process:
            for line in process.stdout:
                fields = line.rstrip(b"\n").split(b"\t")
                start = int(fields[3]) - 1
                if any(lo <= start < hi for lo, hi in windows[fields[2]]):
                    yield fields
            if process.wait() != 0:
                raise SystemExit("samtools view failed")


def measure(bam):
    flags, mapping_qualities, insertions, deletions = (Counter() for _ in range(4))
    aligned_bases = mismatches = quality_bases = quality_sum = 0
    for fields in sample_alignments(bam, sample_windows(bam)):
        flag, mapq = int(fields[1]), int(fields[4])
        mapping_qualities[mapq] += 1
        if mapq != MAPQ:
            continue
        flags[flag] += 1

        [md] = [tag[5:] for tag in fields[11:] if tag.startswith(b"MD:Z:")]
        mismatches += sum(len(change) == 1 for change in MD_CHANGE.findall(md))

        qualities = fields[10]
        query_offset = 0
        for length, op in CIGAR_OP.findall(fields[5]):
            length = int(length)
            if op in (b"M", b"=", b"X"):
                aligned_bases += length
            if op == b"I":
                insertions[length] += 1
            if op == b"D":
                deletions[length] += 1
            if op in (b"M", b"=", b"X", b"I", b"S"):
                if qualities != b"*" and op != b"S":
                    quality_bases += length
                    quality_sum += sum(qualities[query_offset : query_offset + length]) - 33 * length
                query_offset += length

    return {
        "mapq_counts": dict(sorted(mapping_qualities.items())),
        "flag_counts_at_mapq_60": dict(sorted(flags.items())),
        "indel_buckets": [
            [lo, hi, sum(insertions[n] for n in range(lo, hi + 1)), sum(deletions[n] for n in range(lo, hi + 1))]
            for lo, hi in INDEL_BUCKETS
        ],
        "aligned_bases": aligned_bases,
        "mismatches": mismatches,
        "quality_bases": quality_bases,
        "quality_sum": quality_sum,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bam", help="indexed HG002_public_5kHz_sup_v5.2r2_5mC.bam")
    args = parser.parse_args()
    print(json.dumps(measure(args.bam), indent=2))
