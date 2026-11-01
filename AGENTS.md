# AGENTS.md

Notes for coding agents working in this repository. The server is MIT-licensed.

## Overview

`gtfs-mcp` is an operator-agnostic MCP server for GTFS static schedules and optional GTFS-Realtime. The crate and binary are `gtfs-mcp`. The library crate is `gtfs_mcp`. Version `0.2.0` is unpublished. Logs go to stderr so they stay off the stdio JSON-RPC stream.

`GTFS_MCP_STATIC` is required. `GTFS_MCP_TRIP_UPDATES`, `GTFS_MCP_VEHICLE_POSITIONS`, and `GTFS_MCP_ALERTS` are optional. The server does not exchange a username and password for a token. Credentials are extra HTTP headers or a query string already present on a feed URL. See the Authentication section of `README.md`.

## Operator-agnostic

Do not add an agency or operator name, a real feed endpoint, a real feed id, an auth flow, or a special-case code path for one operator in `src/` or `tests/`.

Placeholders belong on reserved domains (`example.com`, `example.org`, `example.net`, `example.test`) or on localhost / `127.0.0.1` / `::1`. `.github/scripts/check-gtfs-urls.sh` fails CI when `src/` or `tests/` contain a GTFS-looking `http(s)` URL on any other host. Spec and license URLs such as `gtfs.org` and the vendored proto header are allowed.

## Setup and commands

Rust 1.88 or newer. `protoc` comes from `protoc-bin-vendored` in `build.rs`. Do not document a system `protoc` install as required.

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
bash scripts/check-versions.sh
bash .github/scripts/check-gtfs-urls.sh
cargo publish --dry-run --locked
```

Run those before a pull request. CI (`.github/workflows/ci.yml`) runs the same checks. Do not merge a red check. Agents never merge pull requests.

## Releasing

`Cargo.toml` is the only version. `scripts/bump-version.sh X.Y.Z` sets that version, every file under `npm/**/package.json`, and the `gtfs-mcp` entry in `Cargo.lock`, then commits `chore(release): vX.Y.Z` and creates an annotated tag. It does not push. Run it only from a clean `main`. Publish with:

```bash
git push --follow-tags
```

Do not invent a version by editing `Cargo.toml` or `package.json` by hand. Do not create a tag any other way. Do not push a tag from a branch other than `main`.

`.github/workflows/release.yml` runs on `v*.*.*`. It checks the tag against `Cargo.toml` and the npm versions, then fmt, clippy, and tests, then publishes. Jobs skip a registry when that version is already there.

Agents do not publish. Do not run `cargo publish`, `npm publish`, `docker push`, or `git push --tags` except for the owner's `git push --follow-tags` from `main` after the bump script. Do not put `CARGO_REGISTRY_TOKEN`, `NPM_TOKEN`, or any other registry token in GitHub Actions secrets. crates.io and npm use GitHub OIDC. The container registry uses `GITHUB_TOKEN` with `packages: write`.

The first crates.io publish and the first npm publish of each package are one-time owner steps, because trusted publishing can be configured only after the package exists. They are documented on the release pull request. After that, only the tag workflow publishes.

A Homebrew tap is not set up. It would need another repository and a token.

## Pull requests

- Branch from `main`. Keep the diff limited to the task.
- Commit messages describe the change in a full sentence.
- Update `README.md` when a user-facing command, tool, or environment variable changes. Update this file when an agent workflow changes.
- Do not commit `target/`, `node_modules/`, staged npm binaries, personal MCP config, or secrets.
