#!/bin/sh
set -eu

cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
if [ "$#" -ne 0 ]; then
    echo "Local gate takes no arguments: CRAP <= 10 is mandatory." >&2
    exit 2
fi
# Keep coverage artifacts separate from other repositories and normal builds.
export CARGO_TARGET_DIR="$PWD/target/local-gate"
exec cargo run --locked -- crap --all --threshold 10 --no-baseline
