#!/usr/bin/env bash
set -euxo pipefail

# Ubuntu's minimal container image may exclude documentation during unpacking.
# Keep this application's docs so its bundled licenses can be verified.
printf '%s\n' 'path-include=/usr/share/doc/ciallogcat/' 'path-include=/usr/share/doc/ciallogcat/*' > /etc/dpkg/dpkg.cfg.d/zz-ciallogcat-docs
apt-get update
apt-get install -y --no-install-recommends "$1" desktop-file-utils
test "$(dpkg-query -W -f='${Status}' ciallogcat)" = "install ok installed"
dpkg-query -L ciallogcat
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
