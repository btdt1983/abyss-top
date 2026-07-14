#!/usr/bin/env bash
# Publish abyss-top RPM(s) into the SHARED, GPG-signed techhack dnf repo
# (/srv/repo), co-existing safely with other tools (vulnscan-ai, cerberus, ...).
#
# Safe-by-design on the shared repo:
#   * Only abyss-top RPMs are signed/copied (PKG_GLOB) — other tools' files are
#     never touched.
#   * createrepo does a FULL scan of each elN/ dir, so every tool already on disk
#     stays in the regenerated metadata (never dropped).
#   * The root index.html is rebuilt from the elN/ dirs that EXIST on disk, so a
#     single-EL publish never drops the other EL's link (the clobber the generic
#     vulnscan script has when run for one dist).
#
# The shared techhack signing key must already exist in the keyring — this
# script never generates an org key.
#
# Env: REPO_ROOT (default /srv/repo), REPO_BASEURL (default https://repo.techhack.nl),
#      RPMS_SRC (dir holding abyss-top RPMs to publish; default rpmbuild RPMS),
#      PKG_GLOB (default 'abyss-top-*'), GPG_EMAIL (default security@techhack.nl),
#      GPG_PASSPHRASE_FILE (0600 file; default /root/.config/vulnscan-ai/signing.pass).
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-/srv/repo}"
REPO_BASEURL="${REPO_BASEURL:-https://repo.techhack.nl}"
RPMS_SRC="${RPMS_SRC:-$(rpm --eval '%{_topdir}')/RPMS}"
PKG_GLOB="${PKG_GLOB:-abyss-top-*}"
GPG_EMAIL="${GPG_EMAIL:-security@techhack.nl}"
GPG_PASSPHRASE_FILE="${GPG_PASSPHRASE_FILE:-/root/.config/vulnscan-ai/signing.pass}"
KEYFILE="RPM-GPG-KEY-techhack"

for t in rpm rpmsign gpg createrepo_c; do
    command -v "$t" >/dev/null || { echo "ERROR: missing '$t'" >&2; exit 1; }
done

# Shared release key must pre-exist; abyss-top does not create org keys.
gpg --list-secret-keys "$GPG_EMAIL" >/dev/null 2>&1 \
    || { echo "ERROR: no secret key for $GPG_EMAIL in keyring — cannot sign." >&2; exit 1; }
KEYID="$(gpg --list-keys --with-colons "$GPG_EMAIL" | awk -F: '/^pub:/{print $5; exit}')"
[ -r "$GPG_PASSPHRASE_FILE" ] \
    || { echo "ERROR: passphrase file not readable: $GPG_PASSPHRASE_FILE" >&2; exit 1; }
GPG_PASS=(--pinentry-mode loopback --passphrase-file "$GPG_PASSPHRASE_FILE")
echo ">> signing key: $KEYID ($GPG_EMAIL)"

# 1. Collect + sign only abyss-top RPMs.
mapfile -t RPMS < <(find "$RPMS_SRC" -name "${PKG_GLOB}.rpm" ! -name '*.src.rpm' | sort)
[ "${#RPMS[@]}" -gt 0 ] || { echo "ERROR: no RPMs matching '${PKG_GLOB}.rpm' under $RPMS_SRC" >&2; exit 1; }
echo ">> signing ${#RPMS[@]} package(s)"
rpmsign --define "_gpg_name $GPG_EMAIL" \
    --define "__gpg_sign_cmd %{__gpg} gpg --no-verbose --no-armor --pinentry-mode loopback --batch --passphrase-file $GPG_PASSPHRASE_FILE -u %{_gpg_name} -sbo %{__signature_filename} --digest-algo sha256 %{__plaintext_filename}" \
    --addsign "${RPMS[@]}"

# 2. Place each RPM into its elN/ dir; full-scan createrepo; detach-sign repomd.
mkdir -p "$REPO_ROOT"
gpg --armor --export "$KEYID" > "$REPO_ROOT/$KEYFILE"
declare -A TOUCHED=()
for rpm in "${RPMS[@]}"; do
    base="$(basename "$rpm")"
    dist="$(printf '%s' "$base" | grep -oE 'el[0-9]+' | head -1)"; dist="${dist:-el9}"
    mkdir -p "$REPO_ROOT/$dist"
    cp -f "$rpm" "$REPO_ROOT/$dist/"
    TOUCHED["$dist"]=1
done
for dist in "${!TOUCHED[@]}"; do
    echo ">> createrepo $dist"
    createrepo_c "$REPO_ROOT/$dist" >/dev/null
    rm -f "$REPO_ROOT/$dist/repodata/repomd.xml.asc"
    gpg --batch --yes "${GPG_PASS[@]}" -u "$KEYID" \
        --detach-sign --armor "$REPO_ROOT/$dist/repodata/repomd.xml"
done

# 3. Client .repo (idempotent; one file covers all ELs via $releasever).
cat > "$REPO_ROOT/techhack.repo" <<EOF
[techhack]
name=techhack tools (EL\$releasever)
baseurl=$REPO_BASEURL/el\$releasever
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$REPO_BASEURL/$KEYFILE
EOF

# 4. Landing pages.
STYLE="<style>body{font-family:system-ui,sans-serif;max-width:48rem;margin:3rem auto;padding:0 1rem;line-height:1.5}code,pre{background:#f4f4f4;padding:.1rem .3rem;border-radius:4px}pre{padding:1rem;overflow:auto}a{color:#0a58ca}</style>"

# Root: rebuilt from the elN/ dirs that EXIST — never drops another EL's link.
mapfile -t ALL_DISTS < <(find "$REPO_ROOT" -maxdepth 1 -type d -name 'el*' -printf '%f\n' | sort)
{
    echo "<!doctype html><meta charset=utf-8><title>techhack RPM repo</title>"
    echo "$STYLE"
    echo "<h1>techhack RPM repository</h1>"
    echo "<p>Signed dnf repository for RHEL-based hosts. Pick your distribution for install instructions:</p>"
    echo "<ul>"
    for dist in "${ALL_DISTS[@]}"; do echo "<li><a href=\"$dist/\">$dist/</a></li>"; done
    echo "</ul>"
    echo "<p><a href=\"$KEYFILE\">GPG public key</a> &middot; <a href=\"techhack.repo\">techhack.repo</a> (covers all versions via \$releasever)</p>"
    echo "<p style=color:#666>Packages are GPG-signed; metadata is signed (repo_gpgcheck).</p>"
} > "$REPO_ROOT/index.html"

# Per-EL page for the dists we touched: list ALL packages in the dir (every
# tool), with a tool-neutral install example.
for dist in "${!TOUCHED[@]}"; do
    {
        echo "<!doctype html><meta charset=utf-8><title>techhack RPM repo ($dist)</title>"
        echo "$STYLE"
        echo "<p><a href=\"../\">&larr; all distributions</a></p>"
        echo "<h1>techhack tools (${dist^^})</h1>"
        echo "<p>Install on ${dist^^}:</p>"
        echo "<pre>sudo rpm --import $REPO_BASEURL/$KEYFILE"
        echo "sudo curl -fsSL -o /etc/yum.repos.d/techhack.repo $REPO_BASEURL/techhack.repo"
        echo "sudo dnf install &lt;package&gt;</pre>"
        echo "<h2>Packages</h2><ul>"
        for pkg in "$REPO_ROOT/$dist/"*.rpm; do
            [ -e "$pkg" ] || continue
            pn="$(basename "$pkg")"; sz="$(du -h "$pkg" | cut -f1)"
            echo "<li><a href=\"$pn\">$pn</a> <span style=color:#666>($sz)</span></li>"
        done
        echo "</ul>"
        echo "<details><summary>Changelog</summary><pre>"
        pkg_first=1
        for pkg in "$REPO_ROOT/$dist/"*.rpm; do
            [ -e "$pkg" ] || continue
            pkgname="$(rpm -qp --qf '%{NAME}' "$pkg" 2>/dev/null)"
            [ "$pkg_first" = 1 ] || echo
            pkg_first=0
            echo "<strong>${pkgname}</strong>"
            rpm -qp --changelog "$pkg" 2>/dev/null \
                | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
        done
        echo "</pre></details>"
        echo "<p style=color:#666>Packages are GPG-signed; metadata is signed (repo_gpgcheck).</p>"
    } > "$REPO_ROOT/$dist/index.html"
done

echo ">> published to $REPO_BASEURL/el\$releasever  (dists on disk: ${ALL_DISTS[*]})"
