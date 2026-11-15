#!/usr/bin/env bash
# Checks the Linux tarball that `cargo xtask release` wrote to <dist> (#50):
# the checksum matches, the layout is what the docs describe (jidr, the
# x86_64 on-device helper next to it, the licences), and the binary runs and
# reports the release's version.
#
# usage: packaging/linux/smoke.sh <dist>
set -euo pipefail

dist="${1:?usage: smoke.sh <dist>}"
version="$(cat "$dist/VERSION")"
name="jidr-${version}-x86_64-unknown-linux-gnu"
tarball="$dist/$name.tar.gz"

(cd "$dist" && sha256sum --check "$name.tar.gz.sha256")

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
tar -xzf "$tarball" -C "$tmp"
for f in jidr helper/x86_64/jidr-helper LICENSE-APACHE NOTICE; do
  [ -f "$tmp/$name/$f" ] || { echo "smoke: $f is missing from $name.tar.gz" >&2; exit 1; }
done
[ -x "$tmp/$name/jidr" ] || { echo "smoke: jidr is not executable" >&2; exit 1; }

# Through a symlink, as the install docs put it on PATH.
mkdir -p "$tmp/bin"
ln -s "$tmp/$name/jidr" "$tmp/bin/jidr"
reported="$("$tmp/bin/jidr" --version)"
[ "$reported" = "jidr $version" ] || { echo "smoke: jidr --version says '$reported', expected 'jidr $version'" >&2; exit 1; }
echo "smoke: $name.tar.gz ok ($reported)"
