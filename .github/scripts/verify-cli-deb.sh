#!/usr/bin/env bash
set -euxo pipefail

# Runs only inside a disposable Ubuntu 24.04 container, never on the host.
package="$1"
test "$(dpkg-deb --field "$package" Package)" = ciallogcat-cli
test "$(dpkg-deb --field "$package" Architecture)" = amd64
dpkg-deb --field "$package" Depends | grep -E '(^|, )libc6 \(>= [0-9]'
test "$(dpkg-deb --field "$package" Recommends)" = adb
if dpkg-deb --field "$package" Depends | grep -Ei 'lib(x11|xcb|xkbcommon|xcursor|xi[0-9]|xrandr|wayland|egl|gl1|glx|opengl|vulkan|gtk|qt)'; then exit 1; fi
mkdir /tmp/cli-payload
dpkg-deb --extract "$package" /tmp/cli-payload
find /tmp/cli-payload -type f -printf '/%P\n' | sort > /tmp/cli-files
printf '%s\n' /usr/bin/ciallogcat-cli /usr/share/doc/ciallogcat-cli/README.md /usr/share/doc/ciallogcat-cli/copyright | sort > /tmp/expected-cli-files
diff -u /tmp/expected-cli-files /tmp/cli-files
# The Ubuntu minimal image can exclude docs; retain this package's license.
printf '%s\n' 'path-include=/usr/share/doc/ciallogcat-cli/' 'path-include=/usr/share/doc/ciallogcat-cli/*' > /etc/dpkg/dpkg.cfg.d/zz-ciallogcat-cli-docs
apt-get update
apt-get install -y --no-install-recommends "$package"
test "$(dpkg-query -W -f='${Status}' ciallogcat-cli)" = 'install ok installed'
test -s /usr/share/doc/ciallogcat-cli/copyright
test -s /usr/share/doc/ciallogcat-cli/README.md
test "$(stat -c '%u:%g:%a' /usr/bin/ciallogcat-cli)" = 0:0:755
/usr/bin/ciallogcat-cli --help
version_output="$(/usr/bin/ciallogcat-cli --version)"
printf '%s\n' "$version_output"
reported_version="$(printf '%s\n' "$version_output" | sed -n 's/.*"version":"\([^" ]*\)".*/\1/p')"
base_version="${reported_version%%+*}"
metadata="${reported_version#"$base_version"}"
deb_version="${base_version/-/~}${metadata}"
test -n "$reported_version"
test "$deb_version" = "$(dpkg-deb --field "$package" Version)"
ldd /usr/bin/ciallogcat-cli > /tmp/cli-ldd
cat /tmp/cli-ldd
if grep -q 'not found' /tmp/cli-ldd; then
  echo 'Unresolved CLI runtime dependencies' >&2
  exit 1
fi
# If supplied, also check actual GUI/CLI coexistence and independent removal.
if [[ $# -gt 1 ]]; then
  apt-get install -y --no-install-recommends "$2"
  test -x /usr/bin/ciallogcat
  /usr/bin/ciallogcat-cli --help >/dev/null
  apt-get purge -y ciallogcat
  test ! -e /usr/bin/ciallogcat
  /usr/bin/ciallogcat-cli --help >/dev/null
  test "$(/usr/bin/ciallogcat-cli --version)" = "$version_output"
  apt-get install -y --no-install-recommends "$2"
fi
apt-get purge -y ciallogcat-cli
test ! -e /usr/bin/ciallogcat-cli
test ! -e /usr/share/doc/ciallogcat-cli
if [[ $# -gt 1 ]]; then
  test -x /usr/bin/ciallogcat
  apt-get purge -y ciallogcat
fi
