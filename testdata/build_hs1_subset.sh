#!/usr/bin/env bash
# Extracts the first ~0.1% of bases of each hs1 contig (whole 50bp lines only,
# per the source .fai's bases-per-line) into testdata/reference/hs1_subset.fa
# and writes its matching .fai. Fetch the source first with
# `bash testdata/fetch.sh DATA_DIR ref`. Tests use the committed subset.
#
# Run from the repo root: testdata/build_hs1_subset.sh
set -euo pipefail

SRC_FA="testdata/large/hs1.fa"
SRC_FAI="testdata/large/hs1.fa.fai"
OUT_FA="testdata/reference/hs1_subset.fa"
OUT_FAI="testdata/reference/hs1_subset.fa.fai"

: > "$OUT_FA"
: > "$OUT_FAI"

out_offset=0
while read -r name length seq_offset bases_per_line bytes_per_line; do
    # chrM (16,569bp) is kept whole rather than shrunk to 0.1% (~17bp) —
    # existing tests fetch chrM at read lengths well past that.
    if [ "$name" = "chrM" ]; then
        target_bases=$length
    else
        target_bases=$(( length / 1000 ))
        if [ "$target_bases" -lt "$bases_per_line" ]; then
            target_bases=$bases_per_line
        fi
    fi
    lines=$(( (target_bases + bases_per_line - 1) / bases_per_line ))
    subset_length=$(( lines * bases_per_line ))
    if [ "$subset_length" -gt "$length" ]; then
        subset_length=$length
        lines=$(( (length + bases_per_line - 1) / bases_per_line ))
    fi
    subset_bytes=$(( lines * bytes_per_line ))

    printf '>%s\n' "$name" >> "$OUT_FA"
    dd if="$SRC_FA" bs=1 skip="$seq_offset" count="$subset_bytes" 2>/dev/null >> "$OUT_FA"
    # Ensure the extracted block ends in a newline (last line of a contig may
    # be short and already newline-terminated by dd's exact byte count).
    tail -c1 "$OUT_FA" | grep -q $'\n' || printf '\n' >> "$OUT_FA"

    header_bytes=$(( ${#name} + 2 ))
    out_seq_offset=$(( out_offset + header_bytes ))
    printf '%s\t%d\t%d\t%d\t%d\n' "$name" "$subset_length" "$out_seq_offset" "$bases_per_line" "$bytes_per_line" >> "$OUT_FAI"
    out_offset=$(( out_seq_offset + subset_bytes ))
done < "$SRC_FAI"

echo "Wrote $OUT_FA and $OUT_FAI"
