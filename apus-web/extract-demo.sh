#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 ]]; then
    echo "Usage: $0 HG002_DATA_DIR OUTPUT.tar.gz" >&2
    exit 2
fi
data=$1
script_dir=$(dirname "$0")
output=$(realpath -m "$2")
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

regions=(chr1:108140703-108243180 chr1:152254195-152354195)
samtools view --no-PG -b -M -o "$stage/demo.bam" \
    "$data/HG002_public_5kHz_sup_v5.2r2_5mC.bam" "${regions[@]}"
cp "$data/HG002_DEL2477_INS975.vcf.gz" "$stage/demo.vcf.gz"
awk '$1 == "chr1"' "$data/hg38.fa.fai" > "$stage/chr1.fai"
test -s "$stage/chr1.fai"

for region in "${regions[@]}"; do
    samtools faidx -n 60 "$data/hg38.fa" "$region"
done | gzip -n > "$stage/windows.fa.gz"

cp "$script_dir"/{data-credits.txt,ONT-MIT-LICENSE.txt} "$stage/"
cp "$script_dir/../testdata/hg002/NIST-DATA-NOTICE.txt" "$stage/"

tar -C "$stage" --format=ustar --owner=0 --group=0 --numeric-owner --mode=0644 --mtime=@0 -cf - \
    chr1.fai demo.bam demo.vcf.gz windows.fa.gz data-credits.txt ONT-MIT-LICENSE.txt NIST-DATA-NOTICE.txt |
    gzip -n > "$output"
sha256sum "$output"
