#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == --help ]]; then
    printf 'Usage: %s DATA_DIR [--bam]\n' "$0"
    exit 0
fi
if (( $# < 1 || $# > 2 )) || [[ $# == 2 && $2 != --bam ]]; then
    printf 'Usage: %s DATA_DIR [--bam]\n' "$0" >&2
    exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
large="${script_dir%/*}/large"
link="$large/hg002"
mkdir -p -- "$1"
cd -P -- "$1"
if [[ -e "$link" && ! -L "$link" && ! "$PWD" -ef "$link" ]]; then
    printf 'Move %s aside before linking %s\n' "$link" "$PWD" >&2
    exit 1
fi

sources="$script_dir/sources.txt"
if [[ -f hg002-sources.txt ]]; then
    sources=hg002-sources.txt
fi

fetch() {
    local file=$1 url
    if [[ -s "$file" ]]; then
        printf 'Using %s\n' "$file"
        return
    fi
    url=$(awk -F '/' -v file="$file" '/^(https?|ftp):\/\// && $NF == file { print; exit }' "$sources")
    if [[ -z "$url" ]]; then
        printf 'Missing URL for %s in %s\n' "$file" "$sources" >&2
        return 1
    fi
    curl --fail --location --retry 3 --continue-at - --output "$file.part" "$url"
    ln -- "$file.part" "$file"
    unlink -- "$file.part"
}

if [[ -s hg38.fa ]]; then
    printf 'Using hg38.fa\n'
else
    fetch hg38.fa.gz
fi
fetch hg38.ncbiRefSeq.gtf.gz
fetch HG002_GRCh38_v5.0q_stvar.vcf.gz
if [[ ${2:-} == --bam ]]; then
    fetch HG002_public_5kHz_sup_v5.2r2_5mC.bam
fi

mkdir -p -- "$large"
if [[ ! "$PWD" -ef "$link" && ! "$PWD" -ef "$large" ]]; then
    ln -sfn -- "$PWD" "$link"
fi
printf 'HG002 data directory: %s\n' "$PWD"
