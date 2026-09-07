#!/usr/bin/env bash
set -euo pipefail

apt-get update
apt-get install -y --no-install-recommends "$1" desktop-file-utils
test "$(dpkg-query -W -f='${Status}' ciallogcat)" = "install ok installed"
test -x /usr/bin/ciallogcat
test -s /usr/share/icons/hicolor/scalable/apps/ciallogcat.svg
test -s /usr/share/doc/ciallogcat/copyright
test -s /usr/share/doc/ciallogcat/fonts/LICENSE-OFL.txt
desktop-file-validate /usr/share/applications/ciallogcat.desktop
ldd /usr/bin/ciallogcat > /tmp/ciallogcat-ldd.txt
cat /tmp/ciallogcat-ldd.txt
if grep -q 'not found' /tmp/ciallogcat-ldd.txt; then
  echo "Unresolved runtime dependencies" >&2
  exit 1
fi
apt-get purge -y ciallogcat
test ! -e /usr/bin/ciallogcat
test ! -e /usr/share/applications/ciallogcat.desktop
test ! -e /usr/share/icons/hicolor/scalable/apps/ciallogcat.svg
