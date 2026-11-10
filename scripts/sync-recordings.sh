#!/bin/bash
# Copy finished (gzipped) recordings from the recorder's folder into
# bullet-data/, which is committed. The hour being written is still a plain
# .csv and is left alone; finished files never change, so each is copied once.
set -euo pipefail
cd "$(dirname "$0")/.."
src=${1:-data/bullet-live}
new=0
for dir in "$src"/*/; do
  market=$(basename "$dir")
  mkdir -p "bullet-data/$market"
  for f in "$dir"*.csv.gz "$dir"*-meta.toml; do
    [ -e "$f" ] || continue
    dest="bullet-data/$market/$(basename "$f")"
    if [ ! -e "$dest" ] || ! cmp -s "$f" "$dest"; then cp "$f" "$dest"; new=$((new + 1)); fi
  done
done
echo "$new new or changed files in bullet-data/"
