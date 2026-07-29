#!/usr/bin/env bash
# Build the offline dictionary artifact hosted as the `dictionary-v1` GitHub
# release asset.
#
# Fetches the pinned Princeton WordNet 3.1 database tarball (WNdb-3.1),
# converts it to Carrel's read-only SQLite schema via build_dictionary_db.py,
# gzips it deterministically, and prints the final `.gz` SHA-256 — that hash
# is what gets baked into the Rust download consts (DICTIONARY_SHA256).
#
# The conversion is deterministic (sorted inserts + VACUUM, gzip -n), so a
# given WNdb-3.1 input always yields the same `.gz` bytes and therefore a
# stable checksum across machines.
#
# Outputs (under build/dictionary/, gitignored):
#   dictionary-v1.db        — decompressed artifact
#   dictionary-v1.db.gz     — the release asset to upload
#
# Release step (run manually once the asset is built, gh authenticated):
#   gh release create dictionary-v1 build/dictionary/dictionary-v1.db.gz \
#       --repo mikedamoiseau/folio --latest=false \
#       --title "Dictionary v1 (WordNet 3.1)" \
#       --notes "Offline dictionary artifact. Definitions from Princeton WordNet 3.1."
#
# Usage:
#   ./scripts/build-dictionary-artifact.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD_DIR="${REPO_ROOT}/build/dictionary"
mkdir -p "${BUILD_DIR}"

# Pinned WNdb-3.1 tarball. The SHA-256 guards against a silent upstream
# republish — a mismatch means Princeton reissued the file and the hash must
# be updated deliberately after review.
#
# NOT YET PINNED: the operator building the artifact must set this to the real
# SHA-256 of the WNdb-3.1 tarball (run the script once with SKIP_PIN=1 to fetch
# and print the hash, verify it against Princeton's published checksum, then
# paste it here). The script refuses to build until it is set.
WNDB_URL="https://wordnetcode.princeton.edu/wn3.1.dict.tar.gz"
# Pinned 2026-07-11. Verified via: canonical HTTPS host (wordnetcode.princeton.edu),
# genuine WNdb-3.1 tarball structure (dict/data.*, index.*, *.exc, dbfiles/), and
# expected ~16.36 MB size. Princeton publishes no per-file digest to compare against
# (their site is a plain directory index), so this is a reviewed pin-on-first-fetch,
# same policy as the pdfium/libmobi pins.
WNDB_SHA256="3f7d8be8ef6ecc7167d39b10d66954ec734280b5bdcd57f7d9eafe429d11c22a"

TARBALL="${BUILD_DIR}/wn3.1.dict.tar.gz"
OUT_DB="${BUILD_DIR}/dictionary-v1.db"
OUT_GZ="${BUILD_DIR}/dictionary-v1.db.gz"

sha256_of() {
  # shasum ships on macOS and most Linux distros; sha256sum is GNU-only but
  # cheaper on Linux CI. Prefer shasum for portability.
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

if [ -z "${WNDB_SHA256}" ] && [ -z "${SKIP_PIN:-}" ]; then
  echo "error: WNDB_SHA256 is not pinned in this script." >&2
  echo "       Run once with SKIP_PIN=1 to fetch the tarball and print its" >&2
  echo "       SHA-256, verify it against Princeton's published checksum, then" >&2
  echo "       set WNDB_SHA256 in this script and re-run." >&2
  exit 1
fi

echo "==> Fetching WNdb-3.1 tarball"
if [ -f "${TARBALL}" ] && [ -n "${WNDB_SHA256}" ] && \
   [ "$(sha256_of "${TARBALL}")" = "${WNDB_SHA256}" ]; then
  echo "  already present with matching checksum"
else
  # -f: fail on 4xx/5xx; -L: follow redirects; -sS: silent but show errors.
  if ! curl -fLsS --retry 5 --retry-delay 3 --retry-all-errors -o "${TARBALL}" "${WNDB_URL}"; then
    echo "error: failed to download WNdb-3.1 from ${WNDB_URL}" >&2
    echo "       The Princeton mirror occasionally moves; verify the URL at" >&2
    echo "       https://wordnet.princeton.edu/download/current-version" >&2
    exit 1
  fi
  actual="$(sha256_of "${TARBALL}")"
  if [ -z "${WNDB_SHA256}" ]; then
    echo "  SKIP_PIN set — fetched tarball SHA-256: ${actual}"
    echo "  Verify this against Princeton's checksum, set WNDB_SHA256, and re-run."
    exit 0
  fi
  if [ "${actual}" != "${WNDB_SHA256}" ]; then
    echo "error: WNdb-3.1 tarball checksum mismatch" >&2
    echo "       expected: ${WNDB_SHA256}" >&2
    echo "       actual:   ${actual}" >&2
    echo "       Upstream may have reissued the file. Verify and update the pin." >&2
    exit 1
  fi
fi

echo "==> Building SQLite artifact"
python3 "${REPO_ROOT}/scripts/build_dictionary_db.py" "${TARBALL}" "${OUT_DB}"

echo "==> Compressing (deterministic gzip)"
# -n: omit filename/timestamp so the gzip stream is byte-stable. -9: max ratio.
gzip -n -9 -c "${OUT_DB}" >"${OUT_GZ}"

GZ_SHA="$(sha256_of "${OUT_GZ}")"
GZ_SIZE="$(wc -c <"${OUT_GZ}" | tr -d ' ')"

echo ""
echo "==> Done"
echo "  artifact : ${OUT_GZ}"
echo "  size     : ${GZ_SIZE} bytes"
echo "  sha256   : ${GZ_SHA}"
echo ""
echo "Bake this into src-tauri (DICTIONARY_SHA256) after uploading the asset:"
echo "  ${GZ_SHA}"
