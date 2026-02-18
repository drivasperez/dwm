#!/bin/sh
set -eu

# Build the static site by substituting {{VERSION}} from Cargo.toml.
# Usage: ./site/build.sh [output_dir]
#   output_dir defaults to site/dist

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -1)"
OUTPUT_DIR="${1:-$REPO_ROOT/site/dist}"

mkdir -p "$OUTPUT_DIR"
sed "s/{{VERSION}}/$VERSION/g" "$REPO_ROOT/site/index.html" > "$OUTPUT_DIR/index.html"
cp -r "$REPO_ROOT/site/public/"* "$OUTPUT_DIR/"

echo "Built site v$VERSION -> $OUTPUT_DIR/index.html"
