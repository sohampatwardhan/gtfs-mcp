#!/usr/bin/env bash
# Fail when src/ or tests/ contain an http(s) URL for a GTFS-looking resource
# on a host outside the reserved allowlist.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

if ! command -v rg >/dev/null 2>&1; then
  echo "ripgrep (rg) is required" >&2
  exit 1
fi

# GTFS-looking http(s) URLs. Spec and license hosts are filtered below.
pattern='https?://[^[:space:]"'\''<>]*((gtfs|feed-set|static\.zip|trip-?updates|vehicle-?positions|/realtime/)[^[:space:]"'\''<>]*|\.pb)'

fail=0
while IFS= read -r line; do
  [[ -z "$line" ]] && continue
  file=${line%%:*}
  rest=${line#*:}
  lineno=${rest%%:*}
  url=${rest#*:}
  url=${url%[),.;\"\']}

  host=$(printf '%s' "$url" | sed -E 's#^[Hh][Tt][Tt][Pp][Ss]?://##')
  host=${host%%/*}
  host=${host%%\?*}
  host=${host%%\#*}
  host=${host##*@}
  if [[ "$host" == \[* ]]; then
    host=${host%%]*}]
  else
    host=${host%%:*}
  fi
  host=$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')

  case "$host" in
    example.com|example.org|example.net|example.test|localhost|127.0.0.1|"[::1]")
      continue
      ;;
    *.example.com|*.example.org|*.example.net|*.example.test)
      continue
      ;;
    gtfs.org|www.gtfs.org|apache.org|www.apache.org)
      continue
      ;;
    developers.google.com|www.w3.org|w3.org|html.spec.whatwg.org)
      continue
      ;;
    github.com)
      shopt -s nocasematch
      if [[ "$url" =~ ^https?://github\.com/google/transit(/|$) ]]; then
        shopt -u nocasematch
        continue
      fi
      shopt -u nocasematch
      ;;
  esac

  echo "$file:$lineno: non-reserved GTFS URL: $url" >&2
  fail=1
done < <(rg -n --no-heading -o -i "$pattern" src tests || true)

if [[ "$fail" -ne 0 ]]; then
  echo "Use example.com, example.org, example.net, example.test, localhost, or 127.0.0.1 for GTFS fixtures." >&2
  exit 1
fi
