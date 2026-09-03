#!/bin/sh
set -eu

repo="spiritledsoftware/lspctl"
release_root="${LSPCTL_RELEASE_ROOT:-https://github.com/$repo/releases}"
version="${LSPCTL_VERSION:-}"

fail() {
  printf 'lspctl installer: %s\n' "$*" >&2
  exit 1
}

for command in curl tar install; do
  command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done

if [ -z "$version" ]; then
  latest_url=$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' "$release_root/latest")
  version=${latest_url##*/tag/v}
fi
version=${version#v}
case "$version" in
  ''|*[!0-9A-Za-z.+-]*) fail "invalid release version: $version" ;;
esac

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) fail "unsupported operating system: $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) fail "unsupported architecture: $(uname -m)" ;;
esac

target="$arch-$os"
archive="lspctl-v$version-$target.tar.gz"
url="$release_root/download/v$version/$archive"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 -fsSL -o "$tmp/$archive" "$url"
curl --proto '=https' --tlsv1.2 -fsSL -o "$tmp/$archive.sha256" "$url.sha256"
expected=$(awk 'NR == 1 { print $1 }' "$tmp/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$archive" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$tmp/$archive" | awk '{ print $1 }')
else
  fail 'sha256sum or shasum is required to verify the download'
fi
[ "$actual" = "$expected" ] || fail 'download checksum mismatch'

tar -xOzf "$tmp/$archive" "lspctl-v$version-$target/lspctl" > "$tmp/lspctl"
install_dir="${LSPCTL_INSTALL_DIR:-${HOME:?HOME must be set}/.local/bin}"
install -d "$install_dir"
install -m 0755 "$tmp/lspctl" "$install_dir/lspctl"
printf 'Installed lspctl %s to %s/lspctl\n' "$version" "$install_dir"

case ":${PATH:-}:" in
  *":$install_dir:"*) ;;
  *) printf 'Add %s to PATH to run lspctl.\n' "$install_dir" ;;
esac
