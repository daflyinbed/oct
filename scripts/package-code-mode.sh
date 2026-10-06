#!/usr/bin/env bash
# Build and package the backend and matching optional code-mode helper.
set -euo pipefail
cd "$(dirname "$0")/.."
export SQLX_OFFLINE="${SQLX_OFFLINE:-true}"
cargo build --locked -p oct-agent -p oct-codemode --features oct-codemode/runtime --release
package_name="oct-code-mode-$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)"
package_dir="${1:-target/package/$package_name}"
if [[ -e "$package_dir" || -e "$package_dir.tar.gz" ]]; then
  printf 'Package destination already exists: %s\n' "$package_dir" >&2
  exit 1
fi
mkdir -p "$(dirname "$package_dir")"
package_staging="$(mktemp -d "$(dirname "$package_dir")/.oct-package-XXXXXX")"
trap 'rm -rf "$package_staging"' EXIT
install -m 755 target/release/oct-agent target/release/oct-codemode-worker "$package_staging/"
install -m 644 README.md "$package_staging/README.md"
target/release/oct-agent --help >/dev/null
(
  cd "$package_staging"
  sha256sum oct-agent oct-codemode-worker README.md > SHA256SUMS
  sha256sum -c SHA256SUMS
)
mv "$package_staging" "$package_dir"
tar -czf "$package_dir.tar.gz" -C "$(dirname "$package_dir")" "$(basename "$package_dir")"
printf 'Packaged: %s.tar.gz\n' "$package_dir"
