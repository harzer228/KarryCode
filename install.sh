#!/bin/sh
set -eu

repo=https://github.com/harzer228/KarryCode
bin="$HOME/.local/bin"
die() { printf '%s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || die "KarryCode requires Linux x64."
[ "$(id -u)" -ne 0 ] || die "Run the installer as your regular user."
[ ! -L "$bin" ] && [ ! -L "$bin/karrycode" ] || die "The install destination is a link."

case "${1:-}" in
    --uninstall)
        rm -f -- "$bin/karrycode"
        printf '%s\n' "KarryCode removed. Your profile was kept."
        exit 0
        ;;
    '') ;;
    *) die 'Usage: install.sh [--uninstall]' ;;
esac

latest=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$repo/releases/latest")
case "$latest" in
    "$repo/releases/tag/"v*) tag=${latest##*/} ;;
    *) die 'Unexpected GitHub release URL.' ;;
esac
case "$tag" in *[!A-Za-z0-9.-]*) die 'Invalid release tag.' ;; esac

mkdir -p -- "$bin"
stage=$(mktemp -d "$bin/.karrycode-install.XXXXXX")
trap 'rm -f -- "$stage/SHA256SUMS" "$stage/karrycode-linux-x64.tar.gz" "$stage/karrycode"; rmdir -- "$stage"' EXIT
archive=karrycode-linux-x64.tar.gz
curl -fsSL "$repo/releases/download/$tag/SHA256SUMS" -o "$stage/SHA256SUMS"
curl -fsSL "$repo/releases/download/$tag/$archive" -o "$stage/$archive"
expected=$(awk -v filename="$archive" '$2 == filename && NF == 2 {print $1}' "$stage/SHA256SUMS")
actual=$(sha256sum "$stage/$archive")
[ "$expected" = "${actual%% *}" ] || die 'SHA-256 mismatch. The installed version was preserved.'
tar -xOzf "$stage/$archive" karrycode > "$stage/karrycode"
chmod 755 "$stage/karrycode"
[ "$("$stage/karrycode" --version)" = "KarryCode CLI ${tag#v}" ] || die 'The downloaded binary does not match the release.'
mv -f -- "$stage/karrycode" "$bin/karrycode"
printf 'Installed KarryCode CLI %s.\n' "${tag#v}"
case ":$PATH:" in
    *":$bin:"*) printf '%s\n' 'Run karrycode to get started.' ;;
    *) printf '%s\n' 'Add ~/.local/bin to PATH in your shell, or run ~/.local/bin/karrycode.' ;;
esac
