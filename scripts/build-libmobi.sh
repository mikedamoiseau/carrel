#!/usr/bin/env bash
# Build libmobi from source into a workspace-local prefix and print the env
# vars Cargo needs to find it. Only macOS and Linux are supported here —
# Windows builds are handled by CI (see .github/workflows/) using libmobi's
# CMake support to produce a static `mobi.lib` that gets baked into the
# release binary. Local Windows dev builds can replicate that recipe by
# hand (CMake + MSVC, BUILD_SHARED_LIBS=OFF, USE_ZLIB=OFF, USE_LIBXML2=OFF).
#
# Use this when you don't want to (or can't) install libmobi system-wide via
# `brew install libmobi` or `apt install libmobi-dev`. It leaves no global
# footprint: the build tree and install prefix live under
# `.libmobi-build/` in the repo root (gitignored).
#
# Override via environment variables:
#   LIBMOBI_VERSION   git tag or branch to check out (default: public)
#   LIBMOBI_PREFIX    install prefix (default: <repo>/.libmobi-build/install)
#   LIBMOBI_SRC_DIR   source checkout dir (default: <repo>/.libmobi-build/src)
#   JOBS              parallel build jobs (default: sysctl/nproc)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD_ROOT="${REPO_ROOT}/.libmobi-build"
SRC_DIR="${LIBMOBI_SRC_DIR:-${BUILD_ROOT}/src}"
PREFIX="${LIBMOBI_PREFIX:-${BUILD_ROOT}/install}"
VERSION="${LIBMOBI_VERSION:-public}"
REPO_URL="https://github.com/bfabiszewski/libmobi.git"

case "$(uname -s)" in
  Darwin) JOBS_DEFAULT="$(sysctl -n hw.ncpu)" ;;
  Linux)  JOBS_DEFAULT="$(nproc)" ;;
  *)
    echo "error: unsupported platform '$(uname -s)'. This script targets macOS and Linux." >&2
    echo "       On Windows, use the CI build (.github/workflows/) or run libmobi's" >&2
    echo "       CMake build manually: cmake -B build -DBUILD_SHARED_LIBS=OFF -DUSE_ZLIB=OFF -DUSE_LIBXML2=OFF" >&2
    exit 1
    ;;
esac
JOBS="${JOBS:-$JOBS_DEFAULT}"

require() {
  local tool="$1"
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: '$tool' is required but was not found in PATH." >&2
    case "$tool" in
      autoreconf|aclocal|autopoint|libtoolize|glibtoolize)
        echo "  macOS: brew install autoconf automake libtool gettext pkg-config" >&2
        echo "  Debian/Ubuntu: apt install autoconf automake libtool gettext pkg-config" >&2
        ;;
      pkg-config) ;;
      git|make) ;;
    esac
    exit 1
  fi
}

require git
require make
require pkg-config
require autoreconf
require aclocal   # from automake; autoreconf invokes it internally
require autopoint # from gettext; invoked by autogen.sh for gettextize

# libtoolize on Linux, glibtoolize on macOS — we only need one.
if ! command -v libtoolize >/dev/null 2>&1 && ! command -v glibtoolize >/dev/null 2>&1; then
  require libtoolize
fi

mkdir -p "${BUILD_ROOT}"

if [ ! -d "${SRC_DIR}/.git" ]; then
  echo "==> Cloning libmobi (${VERSION}) into ${SRC_DIR}"
  git clone --depth 1 --branch "${VERSION}" "${REPO_URL}" "${SRC_DIR}"
else
  echo "==> Updating libmobi checkout in ${SRC_DIR}"
  git -C "${SRC_DIR}" fetch --depth 1 origin "${VERSION}"
  git -C "${SRC_DIR}" checkout "${VERSION}"
  git -C "${SRC_DIR}" reset --hard "origin/${VERSION}" 2>/dev/null || \
    git -C "${SRC_DIR}" reset --hard "${VERSION}"
fi

cd "${SRC_DIR}"

if [ ! -f configure ]; then
  echo "==> Running autogen.sh"
  ./autogen.sh
fi

echo "==> Configuring (prefix=${PREFIX})"
./configure --prefix="${PREFIX}"

echo "==> Building (jobs=${JOBS})"
make -j"${JOBS}"

echo "==> Installing to ${PREFIX}"
make install

PC_FILE="${PREFIX}/lib/pkgconfig/libmobi.pc"
if [ ! -f "${PC_FILE}" ]; then
  echo "error: install completed but ${PC_FILE} is missing." >&2
  exit 1
fi

cat <<EOF

==> libmobi installed to ${PREFIX}

To build Carrel with MOBI support using this local libmobi, export one of the
following in your shell before running 'cargo' or 'npm run tauri':

  # Option A — pkg-config (preferred):
  export PKG_CONFIG_PATH="${PREFIX}/lib/pkgconfig:\${PKG_CONFIG_PATH:-}"

  # Option B — explicit include/lib dirs:
  export LIBMOBI_INCLUDE_DIR="${PREFIX}/include"
  export LIBMOBI_LIB_DIR="${PREFIX}/lib"

At runtime the dynamic loader needs to find libmobi.dylib / libmobi.so:

EOF

case "$(uname -s)" in
  Darwin)
    echo "  export DYLD_LIBRARY_PATH=\"${PREFIX}/lib:\${DYLD_LIBRARY_PATH:-}\""
    ;;
  Linux)
    echo "  export LD_LIBRARY_PATH=\"${PREFIX}/lib:\${LD_LIBRARY_PATH:-}\""
    ;;
esac

echo
echo "Then: cargo build -p folio-core --features mobi"
