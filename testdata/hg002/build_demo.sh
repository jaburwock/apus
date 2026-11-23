#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 && ! ( $# == 2 && $2 == --multisample ) ]]; then
    printf 'Usage: %s HG002_DATA_DIR [--multisample]\n' "$0" >&2
    exit 2
fi

data=$1
fixtures=$(dirname -- "$0")
demo=$fixtures/demo
stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT

bam=$data/HG002_public_5kHz_sup_v5.2r2_5mC.bam
vcf=$data/HG002_GRCh38_v5.0q_stvar.ncbiRefSeq-cds-overlap.sampled-indels.min5x.vcf.gz
reference=hg38-demo.fa.gz
windows=(chr1:108140703-108243180 chr1:152254195-152354195)
regions=(chr1:108190703-108193180 chr1:152304195-152304195)
if [[ ${2:-} == --multisample ]]; then
    demo=$fixtures/multisample
    reference=hg38-multisample.fa.gz
    windows+=(chr2:130913905-131013905)
    regions+=(chr2:130963905-130963905)
fi

python3 - "$data/hg38.fa" "${windows[@]}" <<'PY' | bgzip -c > "$stage/$reference"
import subprocess
import sys

reference, *windows = sys.argv[1:]
with open(reference + ".fai") as index:
    lengths = {fields[0]: int(fields[1]) for fields in map(str.split, index)}

windows_by_contig = {}
for region in windows:
    contig, span = region.split(":")
    first, last = map(int, span.split("-"))
    windows_by_contig.setdefault(contig, []).append((region, first, last))

out = sys.stdout.buffer
for contig, regions in windows_by_contig.items():
    length = lengths[contig]
    sequence = bytearray(b"N") * length
    for region, first, last in regions:
        _, bases = subprocess.check_output(["samtools", "faidx", "-n", "0", reference, region]).splitlines()
        if len(bases) != last - first + 1:
            raise ValueError(f"Unexpected reference length for {region}")
        sequence[first - 1:last] = bases
    out.write(f">{contig}\n".encode())
    out.writelines(sequence[start:start + 60] + b"\n" for start in range(0, length, 60))
PY
samtools faidx "$stage/$reference"

anchors=("${regions[@]%-*}")
sites=$(IFS=,; printf '%s' "${anchors[*]}")
if [[ ${2:-} != --multisample ]]; then
    stem=$stage/HG002_DEL2477_INS975
    bcftools view --no-version -t "$sites" -Oz --write-index=tbi -o "$stem.vcf.gz" "$vcf"
    samtools view --no-PG -b -M -o "$stem.bam" "$bam" "${regions[@]}"
    samtools index "$stem.bam"
    mv -- "$stage"/* "$demo/"
    exit
fi

samples=(del2477 ins975 ins55)
for i in "${!samples[@]}"; do
    sample=${samples[i]}
    # The source program chain is tens of megabytes per BAM header.
    samtools view --no-PG -h -x PG "$bam" "${regions[i]}" |
        awk -v sample="$sample" '
            $1 == "@PG" { next }
            $1 == "@RG" { sub(/\tSM:[^\t]*/, ""); $0 = $0 "\tSM:" sample }
            { print }
        ' |
        samtools view --no-PG -b -o "$stage/$sample.bam" -
    samtools index "$stage/$sample.bam"
done

replicate_of=ins55
replicate=${replicate_of}_rep
samtools view --no-PG -H "$stage/$replicate_of.bam" |
    awk -v sample="$replicate" '$1 == "@RG" { sub(/\tSM:[^\t]*/, "\tSM:" sample) } { print }' \
    > "$stage/replicate.sam"
samtools reheader -P "$stage/replicate.sam" "$stage/$replicate_of.bam" > "$stage/$replicate.bam"
samtools index "$stage/$replicate.bam"
rm -- "$stage/replicate.sam"

bcftools query -r "$sites" --regions-overlap pos \
    -f '%CHROM\t%POS\t%ID\t%REF\t%ALT\t%QUAL\t%FILTER\t%INFO/SVTYPE\t%INFO/SVLEN[\t%GT]\n' "$vcf" \
    > "$stage/records.tsv"
python3 - "$stage/records.tsv" "$stage/$reference.fai" "$sites" "$replicate_of" "$replicate" "${samples[@]}" <<'PY' |
import sys

records_path, index_path, sites, replicate_of, replicate, *samples = sys.argv[1:]
replicate_index = samples.index(replicate_of)
with open(records_path) as records:
    rows = [line.rstrip("\n").split("\t") for line in records]
if [f"{row[0]}:{row[1]}" for row in rows] != sites.split(","):
    raise ValueError("Expected exactly one benchmark indel at each demo site")

print("##fileformat=VCFv4.2")
print("##source=HG002_GRCh38_v5.0q_stvar")
with open(index_path) as index:
    for line in index:
        contig, length, *_ = line.split()
        print(f"##contig=<ID={contig},length={length}>")
print('##INFO=<ID=SVTYPE,Number=1,Type=String,Description="Structural variant type">')
print('##INFO=<ID=SVLEN,Number=1,Type=Integer,Description="Structural variant length">')
print('##FORMAT=<ID=GT,Number=1,Type=String,Description="Genotype">')
print("#CHROM", "POS", "ID", "REF", "ALT", "QUAL", "FILTER", "INFO", "FORMAT", *samples, replicate, sep="\t")
for i, row in enumerate(rows):
    chrom, pos, variant_id, ref, alt, qual, filt, kind, size, gt = row
    calls = ["./."] * len(samples)
    calls[i] = gt.replace("|", "/")
    calls.append(calls[replicate_index])
    print(chrom, pos, variant_id, ref, alt, qual, filt, f"SVTYPE={kind};SVLEN={size}", "GT", *calls, sep="\t")
PY
    bcftools view --no-version -Oz --write-index=tbi -o "$stage/hg002-multisample.vcf.gz"
rm -- "$stage/records.tsv"

mkdir -p -- "$demo"
mv -- "$stage"/* "$demo/"
