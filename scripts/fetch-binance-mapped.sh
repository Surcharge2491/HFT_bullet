#!/bin/bash
# Download Binance's archived trades for every market in bullet-data/binance-map.toml.
# usage: scripts/fetch-binance-mapped.sh FROM TO   (YYYY-MM-DD, inclusive)
set -euo pipefail
cd "$(dirname "$0")/.."
from=$1; to=${2:-$1}
for sym in $(grep -o 'binance = "[A-Z0-9]*"' bullet-data/binance-map.toml | cut -d'"' -f2); do
  cargo run --release -q -- fetch-binance --market perp --symbol "$sym" --dataset agg-trades \
    --from "$from" --to "$to" --out data
done
