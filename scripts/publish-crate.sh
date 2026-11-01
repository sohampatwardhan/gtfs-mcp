#!/usr/bin/env bash
# Publish the crate when this version is not already on crates.io.
# A version that appears during publish (index lag) is success, not failure.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

check_only=0
if [[ "${1:-}" == "--check-only" ]]; then
  check_only=1
fi

version=$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -n 1)
if [[ -z "$version" ]]; then
  echo "Cargo.toml has no X.Y.Z version" >&2
  exit 1
fi

ua="gtfs-mcp (${version}; +https://github.com/sohampatwardhan/gtfs-mcp)"

version_on_index() {
  local code
  code=$(curl -sS -A "$ua" -o /tmp/gtfs-mcp-crate-version.json -w '%{http_code}' \
    "https://crates.io/api/v1/crates/gtfs-mcp/${version}" || true)
  if [[ "$code" == "200" ]]; then
    return 0
  fi
  # Sparse index: one JSON object per line, with "vers".
  if curl -fsS -A "$ua" "https://index.crates.io/gt/fs/gtfs-mcp" 2>/dev/null \
    | grep -q "\"vers\":\"${version}\""; then
    return 0
  fi
  return 1
}

if version_on_index; then
  echo "gtfs-mcp@${version} is already on crates.io; skipping publish"
  exit 0
fi

if [[ "$check_only" -eq 1 ]]; then
  echo "gtfs-mcp@${version} is not on crates.io"
  exit 1
fi

export CARGO_TERM_COLOR=never
set +e
log=$(cargo publish --locked 2>&1)
status=$?
set -e
printf '%s\n' "$log"
if [[ "$status" -eq 0 ]]; then
  exit 0
fi
if printf '%s\n' "$log" | grep -q 'already exists'; then
  echo "gtfs-mcp@${version} is already on crates.io; treating publish as success"
  exit 0
fi
exit "$status"
