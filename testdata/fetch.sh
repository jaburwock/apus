#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bash testdata/fetch.sh DATA_DIR [group ...]
  ref      T2T-CHM13 reference FASTA and samtools index
  gtf      NCBI RefSeq gene annotations for hs1
  clinvar  ClinVar VCF remapped to hs1 and its tabix index
  all      All of the above (default)
EOF
}

if (( $# == 0 )) || [[ $1 == --help ]]; then
    usage
    exit 0
fi

data_dir=$1
shift
if (( $# == 0 )); then
    set -- all
fi

for group in "$@"; do
    case "$group" in
        ref|gtf|clinvar|all) ;;
        *)
            printf 'Unknown fixture group: %s\n' "$group" >&2
            usage >&2
            exit 1
            ;;
    esac
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
large="$script_dir/large"
mkdir -p -- "$data_dir" "$large"
cd -P -- "$data_dir"

temp_dir=
trap '[[ -z "$temp_dir" ]] || rm -rf -- "$temp_dir"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

ucsc=https://hgdownload.soe.ucsc.edu/goldenPath/hs1/bigZips
ensembl=https://ftp.ensembl.org/pub/rapid-release/species/Homo_sapiens/GCA_009914755.4/ensembl/variation/2022_10/vcf/2024_07
clinvar=clinvar_20240624_GCA_009914755.4.vcf.gz

stage_files() {
    if [[ -z "$temp_dir" ]]; then
        temp_dir=$(mktemp -d .fetch.XXXXXX)
    fi
}

check_links() {
    local file
    for file in "$@"; do
        if [[ -e "$large/$file" && ! -L "$large/$file" && ! "$file" -ef "$large/$file" ]]; then
            printf 'Move %s to %s before fetching this group\n' "$large/$file" "$PWD" >&2
            return 1
        fi
    done
}

link_files() {
    local file
    for file in "$@"; do
        if [[ -f "$file" && ! "$file" -ef "$large/$file" ]]; then
            ln -sfn -- "$PWD/$file" "$large/$file"
        fi
    done
}

download() {
    local base_url=$1 filename=$2 hash
    local -a checksum
    if [[ -e "$filename" ]]; then
        printf '%s already exists; skipping download\n' "$filename"
        return
    fi

    hash=$(awk -v filename="$filename" '$2 == filename { print $1 }' "$script_dir/large.sha256")
    if [[ -z "$hash" ]]; then
        printf 'Missing SHA-256 for %s in large.sha256\n' "$filename" >&2
        return 1
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        checksum=(sha256sum)
    elif command -v shasum >/dev/null 2>&1; then
        checksum=(shasum -a 256)
    else
        printf 'Need sha256sum or shasum to verify %s\n' "$filename" >&2
        return 1
    fi

    stage_files
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --retry 3 --output "$temp_dir/$filename" "$base_url/$filename"
    elif command -v wget >/dev/null 2>&1; then
        wget --tries=4 --output-document="$temp_dir/$filename" "$base_url/$filename"
    else
        printf 'Need curl or wget to download %s\n' "$filename" >&2
        return 1
    fi

    # Publish each file only after its checksum passes, so a retry can safely skip it.
    (cd "$temp_dir" && printf '%s  %s\n' "$hash" "$filename" | "${checksum[@]}" -c -)
    mv -- "$temp_dir/$filename" "$filename"
}

fetch_ref() {
    check_links hs1.fa.gz hs1.fa hs1.fa.fai
    if [[ ! -e hs1.fa.fai ]] && ! command -v samtools >/dev/null 2>&1; then
        printf 'Need samtools to index hs1.fa\n' >&2
        return 1
    fi

    if [[ -e hs1.fa ]]; then
        printf 'hs1.fa already exists; skipping decompression\n'
    else
        download "$ucsc" hs1.fa.gz
        stage_files
        gzip -dc hs1.fa.gz > "$temp_dir/hs1.fa"
        mv -- "$temp_dir/hs1.fa" hs1.fa
    fi
    if [[ -e hs1.fa.fai ]]; then
        printf 'hs1.fa.fai already exists; skipping samtools faidx\n'
    else
        stage_files
        samtools faidx --fai-idx "$temp_dir/hs1.fa.fai" hs1.fa
        mv -- "$temp_dir/hs1.fa.fai" hs1.fa.fai
    fi
    link_files hs1.fa.gz hs1.fa hs1.fa.fai
}

fetch_gtf() {
    check_links hs1.ncbiRefSeq.gtf.gz
    download "$ucsc/genes" hs1.ncbiRefSeq.gtf.gz
    link_files hs1.ncbiRefSeq.gtf.gz
}

fetch_clinvar() {
    check_links "$clinvar" "$clinvar.tbi"
    download "$ensembl" "$clinvar"
    download "$ensembl" "$clinvar.tbi"
    link_files "$clinvar" "$clinvar.tbi"
}

for group in "$@"; do
    case "$group" in
        ref) fetch_ref ;;
        gtf) fetch_gtf ;;
        clinvar) fetch_clinvar ;;
        all) fetch_ref; fetch_gtf; fetch_clinvar ;;
    esac
done

printf 'Requested fixture groups are available in %s\n' "$large"
