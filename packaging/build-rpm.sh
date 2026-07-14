#!/usr/bin/env bash
# Build the abyss-top binary RPM natively on the CURRENT host, running the
# spec's %check (cargo test). Used directly for the host EL, and inside an
# almalinux:<N> container by release.sh for other ELs — so each RPM gets that
# EL's dist tag (.elN) and links against that EL's glibc.
#
# This is not a git repo, so the source tarball is staged from the working tree.
#
# Env:
#   OUTDIR   if set, the built binary RPM is copied here (dir is created).
# Prints the path of the built binary RPM on stdout (last line).
set -euo pipefail
cd "$(dirname "$0")/.."                       # project root

NAME=abyss-top
SPEC=packaging/abyss-top.spec
VERSION="$(sed -n 's/^Version:[[:space:]]*//p' "$SPEC" | head -1)"
[ -n "$VERSION" ] || { echo "ERROR: could not read Version from $SPEC" >&2; exit 1; }
# Numerieke release-prefix vóór de %{?dist}-macro (bv. "2%{?dist}" -> "2").
# Vroeger hardcoded als "1" hier — brak zodra Release ooit werd opgehoogd:
# als toevallig nog een oude "-1."-RPM van een vorige build op schijf stond,
# vond de [ -f "$RPM" ]-check die stille, VERKEERDE (stale) RPM i.p.v. de
# zojuist gebouwde, en release.sh (dat enkel op de OUTDIR-copy hieronder
# vertrouwt) zou die stale RPM zonder waarschuwing hebben gepubliceerd.
RELEASE="$(sed -n 's/^Release:[[:space:]]*//p' "$SPEC" | head -1 | sed 's/%{?dist}//')"
[ -n "$RELEASE" ] || { echo "ERROR: could not read Release from $SPEC" >&2; exit 1; }

for t in cargo rpmbuild; do
    command -v "$t" >/dev/null || { echo "ERROR: missing '$t' in PATH" >&2; exit 1; }
done

command -v rpmdev-setuptree >/dev/null && rpmdev-setuptree \
    || mkdir -p "$HOME"/rpmbuild/{SOURCES,SPECS,BUILD,RPMS,SRPMS}
TOP="$(rpm --eval '%{_topdir}')"

# Stage a clean source tarball (only what the build needs — never target/ or dist/).
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
STAGE="$WORK/${NAME}-${VERSION}"
mkdir -p "$STAGE"
cp -a Cargo.toml Cargo.lock README.md LICENSE src packaging "$STAGE"/
tar czf "$TOP/SOURCES/${NAME}-${VERSION}.tar.gz" -C "$WORK" "${NAME}-${VERSION}"
cp -f "$SPEC" "$TOP/SPECS/"

# --nodeps: cargo/rust may be provided by rustup (not an RPM), which the
# BuildRequires check can't see; the real toolchain was verified above.
echo ">> rpmbuild ${NAME}-${VERSION} ($(cargo --version))" >&2
rpmbuild -ba --nodeps "$TOP/SPECS/${NAME}.spec" >&2

DIST="$(rpm --eval '%{?dist}')"; DIST="${DIST#.}"        # el9 / el10 / ...
RPM="$TOP/RPMS/x86_64/${NAME}-${VERSION}-${RELEASE}.${DIST}.x86_64.rpm"
[ -f "$RPM" ] || { echo "ERROR: expected RPM not found: $RPM" >&2; exit 1; }

if [ -n "${OUTDIR:-}" ]; then
    mkdir -p "$OUTDIR"
    cp -f "$RPM" "$OUTDIR/"
fi
echo "$RPM"
