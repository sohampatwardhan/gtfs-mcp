#!/usr/bin/env bash
# Publish the unscoped gtfs-mcp package to npmjs.org when this version is
# not already there. Safe to rerun. Does not publish platform packages.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

version=$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -n 1)
name=$(python3 -c 'import json; print(json.load(open("npm/gtfs-mcp/package.json"))["name"])')
if [[ "$name" != "gtfs-mcp" ]]; then
  echo "npm/gtfs-mcp must stay the unscoped gtfs-mcp package (found ${name})" >&2
  exit 1
fi

# setup-node@v4 used to inject this when registry-url was set. npm 11 rejects it,
# and the paired placeholder token hides OIDC trusted publishing.
if [[ -n "${NPM_CONFIG_USERCONFIG:-}" && -f "$NPM_CONFIG_USERCONFIG" ]]; then
  grep -v '^always-auth=' "$NPM_CONFIG_USERCONFIG" > "${NPM_CONFIG_USERCONFIG}.tmp" || true
  mv "${NPM_CONFIG_USERCONFIG}.tmp" "$NPM_CONFIG_USERCONFIG"
fi
if [[ "${NODE_AUTH_TOKEN:-}" == "XXXXX-XXXXX-XXXXX-XXXXX" ]]; then
  unset NODE_AUTH_TOKEN
fi

if npm view "${name}@${version}" version >/dev/null 2>&1; then
  echo "${name}@${version} is already on npm"
  exit 0
fi

echo "publishing ${name}@${version}"
(cd npm/gtfs-mcp && npm publish --provenance --access public --ignore-scripts)
