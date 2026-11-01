#!/usr/bin/env bash
# Set Cargo.toml, Cargo.lock, and every npm package.json to X.Y.Z,
# then commit and tag. Does not push.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

dry=0
if [[ "${1:-}" == "--dry-run" ]]; then
  dry=1
  shift
fi

version="${1:-}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: scripts/bump-version.sh [--dry-run] X.Y.Z" >&2
  exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
  echo "working tree is not clean" >&2
  exit 1
fi
if [[ "$dry" -eq 0 ]]; then
  branch=$(git rev-parse --abbrev-ref HEAD)
  if [[ "$branch" != "main" ]]; then
    echo "refusing to tag from ${branch}; check out main first" >&2
    exit 1
  fi
fi

python3 - "$version" <<'PY'
import json
import pathlib
import re
import sys

version = sys.argv[1]
cargo = pathlib.Path("Cargo.toml")
text = cargo.read_text()
updated, count = re.subn(
    r'(?m)^version = "[0-9]+\.[0-9]+\.[0-9]+"',
    f'version = "{version}"',
    text,
    count=1,
)
if count != 1:
    raise SystemExit("could not update the Cargo.toml version")
cargo.write_text(updated)

lock = pathlib.Path("Cargo.lock")
lock_text = lock.read_text()
lock_updated, lock_count = re.subn(
    r'(\[\[package\]\]\nname = "gtfs-mcp"\nversion = ")[0-9]+\.[0-9]+\.[0-9]+(")',
    rf"\g<1>{version}\2",
    lock_text,
    count=1,
)
if lock_count != 1:
    raise SystemExit("could not update the gtfs-mcp version in Cargo.lock")
lock.write_text(lock_updated)

for path in sorted(pathlib.Path("npm").rglob("package.json")):
    data = json.loads(path.read_text())
    data["version"] = version
    if "optionalDependencies" in data:
        data["optionalDependencies"] = {
            name: version for name in data["optionalDependencies"]
        }
    path.write_text(json.dumps(data, indent=2) + "\n")
    print(f"set {path} to {version}")
PY

bash scripts/check-versions.sh

if [[ "$dry" -eq 1 ]]; then
  git diff --stat
  git checkout -- Cargo.toml Cargo.lock npm
  echo "dry run only; working tree restored"
  exit 0
fi

git add Cargo.toml Cargo.lock npm
git commit -m "chore(release): v${version}"
git tag -a "v${version}" -m "v${version}"
echo "tagged v${version} on main; publish with: git push --follow-tags"
