#!/usr/bin/env bash
# Fetch the MOBI test corpus used by carrel-core's fixture-gated tests.
#
# The corpus lives under `src-tauri/test-fixtures/` (gitignored) and is
# pulled on demand rather than checked in — MOBI files are a few hundred
# KB each, but keeping a binary corpus in git quickly becomes a
# licensing and reviewability headache. Everything fetched here is in
# the public domain (Project Gutenberg).
#
# Files produced:
#   src-tauri/test-fixtures/alice.mobi         — KF8 (AZW3 / file version 8)
#   src-tauri/test-fixtures/alice-legacy.mobi  — legacy Mobipocket (version 6)
#
# Idempotent: an existing file with matching name is left alone so re-running
# the script after a partial network failure only fetches what's missing.
#
# Usage:
#   ./scripts/fetch-mobi-test-corpus.sh
#   FORCE=1 ./scripts/fetch-mobi-test-corpus.sh   # re-download existing files

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURES_DIR="${REPO_ROOT}/src-tauri/test-fixtures"
mkdir -p "${FIXTURES_DIR}"

# Project Gutenberg IDs and file slugs. Alice's Adventures in Wonderland
# (PG #11) is the canonical test book — it has illustrations, a cover,
# multiple chapters, and is small enough to keep fixture runs fast.
#
# Project Gutenberg publishes both the KF8 and legacy variants under
# different suffixes. We keep the mapping explicit so a broken upstream
# URL shows up here rather than as a cryptic test failure.
# The `-kf8` suffix is load-bearing: `pg11-images.mobi` and
# `pg11-images-kf8.mobi` both resolve 200 OK at Gutenberg, but only the
# latter is KF8 / file-version 8 — the unsuffixed URL returns legacy
# Mobipocket v6 wrapped in a slightly newer header. The KF8 variant is
# what we actually need to exercise the AZW3 code paths, so we fetch
# the `-kf8` URL by name rather than relying on the redirect chain.
#
# Format: `<name>|<url>|<sha256>`. The hash pins fixture content so a
# silent upstream republish (re-encoded with a new tool chain, for
# example) surfaces as a checksum failure rather than a mysterious
# smoke-test regression. Re-run with `FORCE=1` and update the hash
# deliberately when Project Gutenberg reissues a file.
declare -a FILES=(
  "alice.mobi|https://www.gutenberg.org/cache/epub/11/pg11-images-kf8.mobi|6749a1b88a96e6901d929ba3257efdf51443707310dad88fb38b58bb9230dd18"
  "alice-legacy.mobi|https://www.gutenberg.org/cache/epub/11/pg11.mobi|c6de8f83459904177eac2623aa653ef57483ed6968078c8fc3d3171f20e06408"
)

sha256_of() {
  # shasum ships on macOS and most Linux distros; sha256sum is GNU-only
  # but cheaper on Linux CI runners. Prefer shasum for portability.
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

fetch_one() {
  local name="$1"
  local url="$2"
  local expected_sha="$3"
  local dest="${FIXTURES_DIR}/${name}"

  if [ -f "${dest}" ] && [ -z "${FORCE:-}" ]; then
    # Verify the existing file still matches the pinned hash — catches
    # on-disk corruption and stale fixtures that a previous run of this
    # script left behind with a now-outdated URL/content mapping.
    local current_sha
    current_sha="$(sha256_of "${dest}")"
    if [ "${current_sha}" = "${expected_sha}" ]; then
      echo "  already present: ${name}"
      return
    fi
    echo "  checksum mismatch for cached ${name}; re-downloading"
  fi

  echo "  downloading ${name} from ${url}"
  local tmp
  tmp="$(mktemp)"
  # -f: fail on 4xx/5xx; -L: follow redirects; -sS: silent but show errors.
  # --retry 5 + --retry-all-errors: absorb transient CI flakes (Gutenberg
  # occasionally 503s under load) without failing the whole job on a
  # single blip. --retry-delay 3 is enough gap for rate limiters to
  # reset without stretching CI wall-time.
  if ! curl -fLsS --retry 5 --retry-delay 3 --retry-all-errors -o "${tmp}" "${url}"; then
    rm -f "${tmp}"
    echo "error: failed to download ${name} from ${url}" >&2
    echo "       — Project Gutenberg URLs occasionally change; inspect" >&2
    echo "       https://www.gutenberg.org/ebooks/11 and update the script." >&2
    exit 1
  fi

  # Cheap sanity check: MOBI files start with a 32-byte PalmDB header
  # whose bytes 60..64 spell one of BOOKMOBI / TEXtREAd. Reject
  # downloads that don't so a redirect-to-HTML or captive-portal page
  # can't silently land in test-fixtures/.
  local magic
  magic="$(dd if="${tmp}" bs=1 count=8 skip=60 2>/dev/null | tr -d '\0')"
  case "${magic}" in
    BOOKMOBI|TEXtREAd)
      ;;
    *)
      rm -f "${tmp}"
      echo "error: ${name} did not download as a MOBI file (magic: '${magic}')" >&2
      echo "       The URL may have redirected to an HTML landing page." >&2
      exit 1
      ;;
  esac

  # Pinned-content check: reject a silently-republished upstream file.
  # Failing here means either Gutenberg reissued the book or the URL
  # is serving a different variant — the correct response is a human
  # reviewing the change and updating the hash, not silently accepting
  # whatever bytes the server sent.
  local actual_sha
  actual_sha="$(sha256_of "${tmp}")"
  if [ "${actual_sha}" != "${expected_sha}" ]; then
    rm -f "${tmp}"
    echo "error: ${name} checksum mismatch" >&2
    echo "       expected: ${expected_sha}" >&2
    echo "       actual:   ${actual_sha}" >&2
    echo "       Upstream may have reissued the file. Verify manually" >&2
    echo "       and update the sha256 in this script." >&2
    exit 1
  fi

  mv "${tmp}" "${dest}"
  echo "  -> ${dest}"
}

echo "Fetching MOBI test corpus into ${FIXTURES_DIR}"
for entry in "${FILES[@]}"; do
  name="${entry%%|*}"
  rest="${entry#*|}"
  url="${rest%%|*}"
  sha256="${rest#*|}"
  fetch_one "${name}" "${url}" "${sha256}"
done
echo "Done."
