#!/bin/sh
# Install a prebuilt gtfs-mcp binary from GitHub Releases.
# GTFS_MCP_VERSION=X.Y.Z selects a release. Unset uses the latest release.
# PREFIX defaults to $HOME/.local (the binary is placed in $PREFIX/bin).
set -eu

repo="sohampatwardhan/gtfs-mcp"
version="${GTFS_MCP_VERSION:-}"
if [ -z "$version" ]; then
  base="https://github.com/${repo}/releases/latest/download"
else
  version=${version#v}
  base="https://github.com/${repo}/releases/download/v${version}"
fi

os=$(uname -s)
arch=$(uname -m)
case "${os}:${arch}" in
  Linux:x86_64) target="x86_64-unknown-linux-musl" ;;
  Linux:aarch64 | Linux:arm64) target="aarch64-unknown-linux-musl" ;;
  Darwin:x86_64) target="x86_64-apple-darwin" ;;
  Darwin:arm64) target="aarch64-apple-darwin" ;;
  *)
    echo "gtfs-mcp: unsupported platform ${os} ${arch}" >&2
    echo "Windows: use scripts/install.ps1" >&2
    exit 1
    ;;
esac

archive="gtfs-mcp-${target}.tar.gz"
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

curl --proto '=https' --tlsv1.2 -fsSL "${base}/${archive}" -o "${tmpdir}/${archive}"
curl --proto '=https' --tlsv1.2 -fsSL "${base}/sha256sums.txt" -o "${tmpdir}/sha256sums.txt"

(
  cd "$tmpdir"
  if command -v sha256sum >/dev/null 2>&1; then
    grep "  ${archive}$" sha256sums.txt | sha256sum -c -
  else
    grep "  ${archive}$" sha256sums.txt | shasum -a 256 -c -
  fi
)

tar -C "$tmpdir" -xzf "${tmpdir}/${archive}"
dest="${PREFIX:-${HOME}/.local}/bin"
mkdir -p "$dest"
if command -v install >/dev/null 2>&1; then
  install -m 755 "${tmpdir}/gtfs-mcp" "${dest}/gtfs-mcp"
else
  cp "${tmpdir}/gtfs-mcp" "${dest}/gtfs-mcp"
  chmod 755 "${dest}/gtfs-mcp"
fi
echo "installed ${dest}/gtfs-mcp"
