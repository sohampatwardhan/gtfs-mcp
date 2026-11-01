#!/usr/bin/env bash
# Publish a scoped copy of the launcher to GitHub Packages.
# The copy is a temp directory. npm/gtfs-mcp/package.json stays unscoped.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [[ -z "${NODE_AUTH_TOKEN:-}" ]]; then
  echo "NODE_AUTH_TOKEN is required (use the job's GITHUB_TOKEN)" >&2
  exit 1
fi

version=$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"$/\1/p' Cargo.toml | head -n 1)
name="@sohampatwardhan/gtfs-mcp"
registry="https://npm.pkg.github.com"

workdir=$(mktemp -d)
npmrc=$(mktemp)
cleanup() {
  rm -rf "$workdir"
  rm -f "$npmrc"
}
trap cleanup EXIT

# Do not set always-auth. npm expands ${NODE_AUTH_TOKEN} from the environment.
cat > "$npmrc" <<'EOF'
@sohampatwardhan:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${NODE_AUTH_TOKEN}
EOF
export NPM_CONFIG_USERCONFIG="$npmrc"

cp -a npm/gtfs-mcp/. "$workdir/"
python3 - "$workdir/package.json" "$version" <<'PY'
import json
import sys

path, version = sys.argv[1], sys.argv[2]
data = json.loads(open(path).read())
if data.get("name") != "gtfs-mcp":
    raise SystemExit(f"refusing to rewrite a package named {data.get('name')}")
if data.get("version") != version:
    raise SystemExit(f"package version {data.get('version')} != Cargo.toml {version}")
data["name"] = "@sohampatwardhan/gtfs-mcp"
data["repository"] = {
    "type": "git",
    "url": "https://github.com/sohampatwardhan/gtfs-mcp",
}
data["publishConfig"] = {"registry": "https://npm.pkg.github.com"}
data.pop("optionalDependencies", None)
with open(path, "w") as handle:
    json.dump(data, handle, indent=2)
    handle.write("\n")
PY

if npm view "${name}@${version}" version --registry "$registry" >/dev/null 2>&1; then
  echo "${name}@${version} is already on GitHub Packages"
  exit 0
fi

echo "publishing ${name}@${version} to ${registry}"
set +e
log=$(cd "$workdir" && npm publish --ignore-scripts --registry "$registry" 2>&1)
status=$?
set -e
printf '%s\n' "$log"
if [[ "$status" -eq 0 ]]; then
  exit 0
fi
if printf '%s\n' "$log" | grep -Eq 'EPUBLISHCONFLICT|previously published|cannot publish over'; then
  echo "${name}@${version} is already on GitHub Packages"
  exit 0
fi
exit "$status"
