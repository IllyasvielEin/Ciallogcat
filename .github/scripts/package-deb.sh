#!/usr/bin/env bash
set -euo pipefail

: "${RELEASE_TAG:?RELEASE_TAG is required}"
desktop-file-validate assets/linux/ciallogcat.desktop
archive="ciallogcat-${RELEASE_TAG}-linux-x86_64.deb"
cargo deb --no-build --locked --output "target/release-dist/$archive"
dpkg-deb --info "target/release-dist/$archive"
test "$(dpkg-deb --field "target/release-dist/$archive" Package)" = ciallogcat
test "$(dpkg-deb --field "target/release-dist/$archive" Architecture)" = amd64
expected_version="$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "ciallogcat") | .version')"
test "$(dpkg-deb --field "target/release-dist/$archive" Version)" = "$expected_version"
dpkg-deb --field "target/release-dist/$archive" Recommends | grep -w adb

# Check in a clean container, without the runner's build dependencies.
docker run --rm \
  -v "$PWD/target/release-dist:/packages:ro" \
  -v "$PWD/.github/scripts/verify-deb.sh:/verify-deb.sh:ro" \
  -e DEBIAN_FRONTEND=noninteractive \
  ubuntu:24.04 bash /verify-deb.sh "/packages/$archive"

(
  cd target/release-dist
  sha256sum "$archive" > "$archive.sha256"
)
