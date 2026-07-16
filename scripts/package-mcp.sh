#!/usr/bin/env bash

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
binary_dir=${1:?"usage: package-mcp.sh <binary-dir> [out-dir]"}
out_dir=${2:-"$repo_root/dist"}
package_name=flashgrep-mcp
package_dir="$out_dir/$package_name"
archive_path="$out_dir/$package_name.zip"

crate_version=$(
  cargo metadata --manifest-path "$repo_root/Cargo.toml" --no-deps --format-version 1 |
    node -e 'const fs = require("node:fs"); const metadata = JSON.parse(fs.readFileSync(0, "utf8")); process.stdout.write(metadata.packages.find((pkg) => pkg.name === "flashgrep").version);'
)
mcp_version=$(node -e 'process.stdout.write(require(process.argv[1]).version)' "$repo_root/mcp/package.json")
if [[ "$mcp_version" != "$crate_version" ]]; then
  echo "version mismatch: flashgrep=$crate_version flashgrep-mcp=$mcp_version" >&2
  exit 1
fi

binaries=(
  flashgrep-aarch64-apple-darwin
  flashgrep-x86_64-apple-darwin
  flashgrep-aarch64-unknown-linux-musl
  flashgrep-x86_64-unknown-linux-musl
  flashgrep-aarch64-pc-windows-msvc.exe
  flashgrep-x86_64-pc-windows-msvc.exe
)

for binary in "${binaries[@]}"; do
  if [[ ! -f "$binary_dir/$binary" ]]; then
    echo "required binary not found: $binary_dir/$binary" >&2
    exit 1
  fi
done

rm -rf "$package_dir"
rm -f "$archive_path"
mkdir -p "$package_dir/bin"
cp -R "$repo_root/mcp/." "$package_dir/"
rm -f "$package_dir/.gitignore"

for binary in "${binaries[@]}"; do
  cp "$binary_dir/$binary" "$package_dir/bin/$binary"
done
chmod +x "$package_dir/bin/flashgrep-"*darwin
chmod +x "$package_dir/bin/flashgrep-"*linux-musl

(
  cd "$out_dir"
  zip -qr "$package_name.zip" "$package_name"
)

echo "created MCP bundle: $archive_path"
