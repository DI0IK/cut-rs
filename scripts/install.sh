#!/usr/bin/env sh
set -eu

REPO="DI0IK/cut-rs"
INSTALL_DIR="${CUT_RS_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *)
        echo "Unsupported Linux architecture: $arch" >&2
        exit 1
        ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64|amd64) target="x86_64-apple-darwin" ;;
      *)
        echo "Unsupported macOS architecture: $arch" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $os (use install.ps1 on Windows)" >&2
    exit 1
    ;;
esac

archive="cut-rs-${target}.tar.gz"
base_url="https://github.com/${REPO}/releases/latest/download"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

echo "Downloading ${archive}..."
curl -fsSL "${base_url}/${archive}" -o "${tmp_dir}/${archive}"

if curl -fsSL "${base_url}/SHA256SUMS.txt" -o "${tmp_dir}/SHA256SUMS.txt" 2>/dev/null; then
  expected="$(grep " ${archive}\$" "${tmp_dir}/SHA256SUMS.txt" | awk '{print $1}' || true)"
  if [ -n "$expected" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "${tmp_dir}/${archive}" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
      actual="$(shasum -a 256 "${tmp_dir}/${archive}" | awk '{print $1}')"
    else
      actual="$expected"
    fi
    if [ "$expected" != "$actual" ]; then
      echo "Checksum verification failed for ${archive}!" >&2
      exit 1
    fi
  fi
fi

mkdir -p "$INSTALL_DIR"
tar -xzf "${tmp_dir}/${archive}" -C "$tmp_dir"
mv "${tmp_dir}/cut-rs" "${INSTALL_DIR}/cut-rs"
chmod +x "${INSTALL_DIR}/cut-rs"

echo "Installed cut-rs to ${INSTALL_DIR}/cut-rs"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "Add ${INSTALL_DIR} to your PATH, e.g.:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac

echo ""
echo "Next step: run 'cut-rs init' to auto-configure your AI coding tools."
