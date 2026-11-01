#!/usr/bin/env bash
# Fail unless every npm package.json version matches Cargo.toml.
# When GITHUB_REF is a vX.Y.Z tag, the tag must match too.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

cargo_version=$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -n 1)
if [[ -z "$cargo_version" ]]; then
  echo "Cargo.toml has no X.Y.Z version" >&2
  exit 1
fi

python3 - "$cargo_version" <<'PY'
import json
import pathlib
import sys

expected = sys.argv[1]
paths = sorted(pathlib.Path("npm").rglob("package.json"))
if paths != [pathlib.Path("npm/gtfs-mcp/package.json")]:
    raise SystemExit(f"expected only npm/gtfs-mcp/package.json, found {paths}")
for path in paths:
    data = json.loads(path.read_text())
    version = data.get("version")
    if version != expected:
        raise SystemExit(f"{path} version is {version}, Cargo.toml is {expected}")
    for name, dep in (data.get("optionalDependencies") or {}).items():
        if dep != expected:
            raise SystemExit(f"{path} optional dependency {name} is {dep}, expected {expected}")
print(f"npm package versions match Cargo.toml {expected}")
PY

ref="${GITHUB_REF:-}"
if [[ "$ref" == refs/tags/v* ]]; then
  tag="${ref#refs/tags/}"
  if [[ "$tag" != "v${cargo_version}" ]]; then
    echo "tag ${tag} does not match Cargo.toml version ${cargo_version}" >&2
    exit 1
  fi
  echo "tag ${tag} matches Cargo.toml"
fi
