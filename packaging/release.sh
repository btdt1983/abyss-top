#!/usr/bin/env bash
# One-shot abyss-top release: build the RPM for each target EL, sign + publish to
# the shared techhack repo, reload nginx. Run on the repo host.
#
#   packaging/release.sh
#
# Env:
#   DISTS         space-separated targets (default "el9 el10")
#   HOST_DIST     the EL this host is (default el9) — built natively, runs %check
#   REPO_ROOT     repo web root (default /srv/repo)
#   REPO_BASEURL  public base (default https://repo.techhack.nl)
#   CONTAINER_RT  podman|docker for non-host ELs (default podman)
#   GPG_PASSPHRASE_FILE  0600 file with the techhack key passphrase
#                        (default /root/.config/vulnscan-ai/signing.pass)
#   SUDO          command prefix for privileged steps (default empty / root)
set -euo pipefail
cd "$(dirname "$0")/.."
HERE="$(pwd)"

DISTS="${DISTS:-el9 el10}"
HOST_DIST="${HOST_DIST:-el9}"
REPO_ROOT="${REPO_ROOT:-/srv/repo}"
REPO_BASEURL="${REPO_BASEURL:-https://repo.techhack.nl}"
RT="${CONTAINER_RT:-podman}"
SUDO="${SUDO:-}"
VERSION="$(sed -n 's/^Version:[[:space:]]*//p' packaging/abyss-top.spec | head -1)"
echo ">> releasing abyss-top $VERSION for: $DISTS"

OUT="$(mktemp -d)"; trap 'rm -rf "$OUT"' EXIT
for dist in $DISTS; do
    if [ "$dist" = "$HOST_DIST" ]; then
        echo ">> [$dist] native build (rpmbuild, runs %check)"
        OUTDIR="$OUT" bash packaging/build-rpm.sh >/dev/null
    else
        echo ">> [$dist] native build in $RT almalinux:${dist#el}"
        command -v "$RT" >/dev/null || { echo "ERROR: $RT not installed (needed for $dist)"; exit 1; }
        # label=disable so the container can read /src and write /out without
        # SELinux relabeling the host tree. build-rpm.sh reads only source files
        # (never target/), so mounting the working tree read-only is fine.
        "$RT" run --rm --security-opt label=disable \
            -v "$HERE":/src:ro -v "$OUT":/out "almalinux:${dist#el}" bash -c '
                set -e
                dnf -y install gcc rpm-build rpmdevtools cargo rust >/dev/null 2>&1
                OUTDIR=/out bash /src/packaging/build-rpm.sh >/dev/null
            '
    fi
done

echo ">> built: $(cd "$OUT" && echo *.rpm)"
echo ">> signing + publishing to $REPO_ROOT"
GPG_PASSPHRASE_FILE="${GPG_PASSPHRASE_FILE:-/root/.config/vulnscan-ai/signing.pass}" \
    RPMS_SRC="$OUT" REPO_ROOT="$REPO_ROOT" REPO_BASEURL="$REPO_BASEURL" PKG_GLOB='abyss-top-*' \
    bash packaging/make-repo.sh

# SELinux label (no-op off SELinux) + validate & reload nginx.
command -v chcon >/dev/null && $SUDO chcon -R -t httpd_sys_content_t "$REPO_ROOT" 2>/dev/null || true
$SUDO nginx -t && $SUDO systemctl reload nginx
echo ">> published abyss-top $VERSION -> $REPO_BASEURL/el\$releasever"
