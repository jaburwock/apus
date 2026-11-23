#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == --help ]]; then
    printf 'Usage: %s DATA_DIR\n' "$0"
    exit 0
fi
if (( $# != 1 )); then
    printf 'Usage: %s DATA_DIR\n' "$0" >&2
    exit 2
fi

cd -- "$1"
gtf=hg38.ncbiRefSeq.gtf.gz
vcf=HG002_GRCh38_v5.0q_stvar.vcf.gz
bam=HG002_public_5kHz_sup_v5.2r2_5mC.bam
for file in "$gtf" "$vcf"; do
    if [[ ! -s "$file" ]]; then
        printf 'Missing input: %s/%s\n' "$PWD" "$file" >&2
        exit 1
    fi
done

stage=$(mktemp -d .hg002-preprocess.XXXXXXXX)
cleanup() {
    rm -f -- "$stage/hg38.fa" "$stage/hg38.fa.fai" "$stage/$vcf.tbi" "$stage/$bam.bai"
    rmdir -- "$stage"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ ! -s hg38.fa ]]; then
    gzip -dc -- hg38.fa.gz > "$stage/hg38.fa"
    mv -- "$stage/hg38.fa" hg38.fa
fi
if [[ ! -s hg38.fa.fai ]]; then
    samtools faidx --fai-idx "$stage/hg38.fa.fai" hg38.fa
    mv -- "$stage/hg38.fa.fai" hg38.fa.fai
fi
if [[ ! -s "$vcf.tbi" ]]; then
    bcftools index --tbi --output "$stage/$vcf.tbi" "$vcf"
    mv -- "$stage/$vcf.tbi" "$vcf.tbi"
fi
if [[ -s "$bam" && ! -s "$bam.bai" ]]; then
    samtools index -b -o "$stage/$bam.bai" "$bam"
    mv -- "$stage/$bam.bai" "$bam.bai"
fi
printf 'Indexes ready in %s\n' "$PWD"
