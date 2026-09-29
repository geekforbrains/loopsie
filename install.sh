#!/bin/sh
set -eu

repo='https://github.com/geekforbrains/loopsie'
version=${LOOPSIE_VERSION:-latest}
install_dir=${LOOPSIE_INSTALL_DIR:-"$HOME/.local/bin"}

case "$version" in
  latest) release_path='releases/latest/download' ;;
  v[0-9]*)
    case "$version" in
      *[!a-zA-Z0-9._-]*) echo "Invalid release version: $version" >&2; exit 1 ;;
    esac
    release_path="releases/download/$version"
    ;;
  *) echo "Invalid release version: $version" >&2; exit 1 ;;
esac

case "$(uname -s):$(uname -m)" in
  Darwin:x86_64) target='x86_64-apple-darwin' ;;
  Darwin:arm64) target='aarch64-apple-darwin' ;;
  Linux:x86_64) target='x86_64-unknown-linux-musl' ;;
  Linux:aarch64|Linux:arm64) target='aarch64-unknown-linux-musl' ;;
  *) echo 'Unsupported OS or CPU architecture (requires macOS or Linux on x86_64 or arm64).' >&2; exit 1 ;;
esac

if ! command -v curl >/dev/null 2>&1; then
  echo 'curl is required to download loopsie.' >&2
  exit 1
fi

archive="loopsie-${target}.tar.gz"
url="${repo}/${release_path}/${archive}"
temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/loopsie.XXXXXXXX")
staged_binary=''
trap 'rm -rf "$temp_dir"; if [ -n "$staged_binary" ]; then rm -f "$staged_binary"; fi' EXIT

curl -fL --retry 3 --silent --show-error "$url" -o "$temp_dir/$archive"
curl -fL --retry 3 --silent --show-error "$url.sha256" -o "$temp_dir/$archive.sha256"

read -r expected_hash _ < "$temp_dir/$archive.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  actual_hash=$(sha256sum "$temp_dir/$archive")
elif command -v shasum >/dev/null 2>&1; then
  actual_hash=$(shasum -a 256 "$temp_dir/$archive")
else
  echo 'sha256sum or shasum is required to verify loopsie.' >&2
  exit 1
fi
actual_hash=${actual_hash%% *}
if [ "$actual_hash" != "$expected_hash" ]; then
  echo "Checksum mismatch for $archive." >&2
  exit 1
fi

tar -xzf "$temp_dir/$archive" -C "$temp_dir" loopsie
mkdir -p "$install_dir"
staged_binary=$(mktemp "$install_dir/.loopsie.XXXXXXXX")
install -m 755 "$temp_dir/loopsie" "$staged_binary"
mv -f "$staged_binary" "$install_dir/loopsie"
staged_binary=''
printf 'Installed loopsie to %s/loopsie\n' "$install_dir"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) printf 'Add %s to your PATH, then run loopsie --version.\n' "$install_dir" ;;
esac
