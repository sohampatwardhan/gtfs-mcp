#!/usr/bin/env bash
# Copy release binaries into the platform packages and publish any version
# that is not already on npm. Safe to rerun.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

artifacts="${1:-artifacts}"
version=$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -n 1)

# setup-node@v4 used to inject this when registry-url was set. npm 11 rejects it,
# and the paired placeholder token hides OIDC trusted publishing.
if [[ -n "${NPM_CONFIG_USERCONFIG:-}" && -f "$NPM_CONFIG_USERCONFIG" ]]; then
  grep -v '^always-auth=' "$NPM_CONFIG_USERCONFIG" > "${NPM_CONFIG_USERCONFIG}.tmp" || true
  mv "${NPM_CONFIG_USERCONFIG}.tmp" "$NPM_CONFIG_USERCONFIG"
fi
if [[ "${NODE_AUTH_TOKEN:-}" == "XXXXX-XXXXX-XXXXX-XXXXX" ]]; then
  unset NODE_AUTH_TOKEN
fi

copy_bin() {
  local slug="$1"
  local src="$2"
  local dest="$3"
  if [[ ! -f "$src" ]]; then
    echo "missing binary ${src}" >&2
    exit 1
  fi
  cp "$src" "$dest"
  if [[ "$dest" != *.exe ]]; then
    chmod 755 "$dest"
  fi
}

copy_bin linux-x64 "${artifacts}/bin-linux-x64/gtfs-mcp" npm/linux-x64/gtfs-mcp
copy_bin linux-arm64 "${artifacts}/bin-linux-arm64/gtfs-mcp" npm/linux-arm64/gtfs-mcp
copy_bin darwin-x64 "${artifacts}/bin-darwin-x64/gtfs-mcp" npm/darwin-x64/gtfs-mcp
copy_bin darwin-arm64 "${artifacts}/bin-darwin-arm64/gtfs-mcp" npm/darwin-arm64/gtfs-mcp
copy_bin win32-x64 "${artifacts}/bin-win32-x64/gtfs-mcp.exe" npm/win32-x64/gtfs-mcp.exe

publish_dir() {
  local dir="$1"
  local name
  name=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["name"])' "${dir}/package.json")
  if npm view "${name}@${version}" version >/dev/null 2>&1; then
    echo "${name}@${version} is already on npm"
    return 0
  fi
  echo "publishing ${name}@${version}"
  (cd "$dir" && npm publish --provenance --access public --ignore-scripts)
}

publish_dir npm/linux-x64
publish_dir npm/linux-arm64
publish_dir npm/darwin-x64
publish_dir npm/darwin-arm64
publish_dir npm/win32-x64
publish_dir npm/gtfs-mcp
