#!/bin/sh
# Install the latest mcpherd release.
#   curl -fsSL https://raw.githubusercontent.com/Rorogogogo/mcpherd/main/install.sh | sh
# Set MCPHERD_INSTALL_DIR to choose where the binary goes (default: ~/.local/bin).
set -eu

repo="Rorogogogo/mcpherd"
dir="${MCPHERD_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "mcpherd: unsupported platform $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

url="https://github.com/$repo/releases/latest/download/mcpherd-$target.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading mcpherd for $target..."
curl -fsSL "$url" -o "$tmp/mcpherd.tar.gz"
curl -fsSL "$url.sha256" -o "$tmp/mcpherd.tar.gz.sha256"

expected="$(cut -d' ' -f1 "$tmp/mcpherd.tar.gz.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/mcpherd.tar.gz" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/mcpherd.tar.gz" | cut -d' ' -f1)"
fi
if [ "$expected" != "$actual" ]; then
  echo "mcpherd: checksum mismatch, aborting" >&2
  exit 1
fi

tar -xzf "$tmp/mcpherd.tar.gz" -C "$tmp"
mkdir -p "$dir"
mv "$tmp/mcpherd" "$dir/mcpherd"
chmod +x "$dir/mcpherd"
echo "Installed mcpherd to $dir/mcpherd"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Note: $dir is not on your PATH. Add it, or run $dir/mcpherd directly." ;;
esac
