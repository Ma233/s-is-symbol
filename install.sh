#!/bin/sh
set -eu

REPO="ma233/s-is-symbol"
VERSION="${SYMBOL_VERSION:-latest}"
BINARY_NAME="s"

say() {
  printf '%s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

INSTALL_DIR="${HOME:+$HOME/.local/bin}"

usage() {
  cat <<EOF
Usage: install.sh [--install-dir PATH]

Set SYMBOL_VERSION to a release tag or prerelease to install a specific version.

EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --install-dir)
      [ "$#" -ge 2 ] || fail "--install-dir requires a value"
      INSTALL_DIR="$2"
      shift
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    *)
      usage >&2
      fail "unknown argument: $1"
      ;;
  esac
  shift
done

[ -n "$INSTALL_DIR" ] || fail "HOME is not set; use --install-dir"

case "$VERSION" in
  latest | prerelease | v[0-9]*.[0-9]*.[0-9]*) ;;
  *) fail "invalid release: $VERSION; expected latest, prerelease, or a v-prefixed semantic version" ;;
esac

need curl
need unzip

if command -v sha256sum >/dev/null 2>&1; then
  checksum_tool="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  checksum_tool="shasum"
else
  fail "missing required command: sha256sum or shasum"
fi

calculate_checksum() {
  case "$checksum_tool" in
    sha256sum) sha256sum "$1" ;;
    shasum) shasum -a 256 "$1" ;;
  esac
}

case "$(uname -s)" in
  Darwin)
    case "$(uname -m)" in
      arm64) target="aarch64-apple-darwin" ;;
      x86_64) fail "no prebuilt binary is published for Intel macOS yet" ;;
      *) fail "unsupported macOS architecture: $(uname -m)" ;;
    esac
    ;;
  Linux)
    case "$(uname -m)" in
      x86_64) target="x86_64-unknown-linux-musl" ;;
      aarch64) target="aarch64-unknown-linux-musl" ;;
      *) fail "unsupported Linux architecture: $(uname -m)" ;;
    esac
    ;;
  *) fail "unsupported operating system: $(uname -s)" ;;
esac

if [ "$VERSION" = "latest" ]; then
  release_url="$(curl --fail --silent --show-error --location \
    --output /dev/null --write-out '%{url_effective}' \
    "https://github.com/${REPO}/releases/latest")" \
    || fail "failed to resolve latest release for ${REPO}"
  VERSION="${release_url##*/}"
  case "$VERSION" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "latest release resolved to an invalid tag: $VERSION" ;;
  esac
fi

asset_name="${BINARY_NAME}-${VERSION}-${target}.zip"
temporary="$(mktemp -d)"
archive="${temporary}/${asset_name}"
checksum="${archive}.sha256"
staged=""

cleanup() {
  if [ -n "$staged" ]; then
    rm -f "$staged"
  fi
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM

say "Downloading ${asset_name} from ${REPO} release ${VERSION}..."
curl --fail --silent --show-error --location \
  --output "$archive" \
  "https://github.com/${REPO}/releases/download/${VERSION}/${asset_name}" \
  || fail "failed to download ${asset_name}"
curl --fail --silent --show-error --location \
  --output "$checksum" \
  "https://github.com/${REPO}/releases/download/${VERSION}/${asset_name}.sha256" \
  || fail "failed to download ${asset_name}.sha256"

[ -f "$archive" ] || fail "release asset was not downloaded: ${asset_name}"
[ -f "$checksum" ] || fail "release checksum was not downloaded: ${asset_name}.sha256"
read -r expected_checksum _ <"$checksum" \
  || fail "release checksum is empty: ${asset_name}.sha256"
[ -n "$expected_checksum" ] \
  || fail "release checksum is empty: ${asset_name}.sha256"
actual_checksum="$(calculate_checksum "$archive")"
actual_checksum="${actual_checksum%%[[:space:]]*}"
[ "$actual_checksum" = "$expected_checksum" ] \
  || fail "checksum mismatch for ${asset_name}"

if ! unzip -p "$archive" "$BINARY_NAME" >"${temporary}/${BINARY_NAME}"; then
  fail "release asset does not contain ${BINARY_NAME}"
fi

chmod 0755 "${temporary}/${BINARY_NAME}"
version_output="$("${temporary}/${BINARY_NAME}" --version 2>/dev/null || true)"
[ -n "$version_output" ] || fail "downloaded s binary did not report a version"

mkdir -p "$INSTALL_DIR"
[ ! -d "$INSTALL_DIR/$BINARY_NAME" ] \
  || fail "$INSTALL_DIR/$BINARY_NAME is a directory and cannot be replaced"
staged="$INSTALL_DIR/.symbol.$$"
mv "${temporary}/${BINARY_NAME}" "$staged"
mv -f "$staged" "$INSTALL_DIR/$BINARY_NAME"
staged=""

say "Installed ${BINARY_NAME} to ${INSTALL_DIR}/${BINARY_NAME}"
say "$version_output"
case ":${PATH:-}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) say "Add ${INSTALL_DIR} to PATH before running ${BINARY_NAME} from a new shell." ;;
esac
