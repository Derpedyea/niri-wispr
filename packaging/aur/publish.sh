#!/usr/bin/env bash
# Publish PKGBUILDs to the AUR. Runs in CI inside archlinux:base-devel;
# also works locally on an Arch system.
#
# Required env:
#   VERSION             release version, with or without the leading v
#   GITHUB_REPOSITORY   owner/repo (e.g. Derpedyea/niri-wispr)
#   AUR_SSH_PRIVATE_KEY private key registered with your AUR account;
#                       if unset the script is a silent no-op
# Optional env:
#   AUR_USERNAME / AUR_EMAIL  git identity for the AUR commits
set -euo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

VERSION="${VERSION:?set VERSION}" # release version; a leading v is stripped
VERSION="${VERSION#v}"
REPO="${GITHUB_REPOSITORY:?set GITHUB_REPOSITORY}"

if [ -z "${AUR_SSH_PRIVATE_KEY:-}" ]; then
  echo "::notice::AUR_SSH_PRIVATE_KEY not set — skipping AUR publish"
  exit 0
fi

# --- SSH auth for aur@aur.archlinux.org ---
install -d -m 700 ~/.ssh
printf '%s\n' "$AUR_SSH_PRIVATE_KEY" > ~/.ssh/aur
chmod 600 ~/.ssh/aur
ssh-keyscan -t ed25519,rsa aur.archlinux.org >> ~/.ssh/known_hosts 2>/dev/null
export GIT_SSH_COMMAND="ssh -i ~/.ssh/aur -o IdentitiesOnly=yes"

git config --global user.name "${AUR_USERNAME:-aur-publish[bot]}"
git config --global user.email "${AUR_EMAIL:-aur-publish@localhost}"

# makepkg refuses to run as root; drop to an unprivileged user for .SRCINFO.
if [ "$(id -u)" -eq 0 ]; then
  id builder >/dev/null 2>&1 || useradd -m builder
  AS_USER="runuser -u builder --"
else
  AS_USER=""
fi

sha256_of() {
  curl -fsSL "$1" | sha256sum | cut -d' ' -f1
}

publish() {
  local pkg="$1" template="$2" src_url="$3" dir
  dir="$(mktemp -d)/$pkg"

  echo "=== $pkg $VERSION ==="
  if ! git clone --depth 1 "ssh://aur@aur.archlinux.org/$pkg.git" "$dir" 2>/dev/null; then
    # Package doesn't exist yet — the first push creates it.
    mkdir -p "$dir"
    git -C "$dir" init -b master
    git -C "$dir" remote add origin "ssh://aur@aur.archlinux.org/$pkg.git"
  fi

  cp "$template" "$dir/PKGBUILD"
  sed -i \
    -e "s/^pkgver=.*/pkgver=$VERSION/" \
    -e "s|^sha256sums=.*|sha256sums=('$(sha256_of "$src_url")')|" \
    "$dir/PKGBUILD"

  chmod -R a+rwX "$dir"
  $AS_USER sh -c "cd '$dir' && makepkg --printsrcinfo" > "$dir/.SRCINFO"

  git -C "$dir" add PKGBUILD .SRCINFO
  if git -C "$dir" diff --cached --quiet; then
    echo "$pkg is already up to date"
    return
  fi
  git -C "$dir" commit -m "v$VERSION"
  git -C "$dir" push origin HEAD:master
  echo "published $pkg $VERSION"
}

publish dictationapp \
  "$SELF_DIR/PKGBUILD" \
  "https://github.com/$REPO/archive/refs/tags/v$VERSION.tar.gz"

publish dictationapp-bin \
  "$SELF_DIR/PKGBUILD-bin" \
  "https://github.com/$REPO/releases/download/v$VERSION/dictationapp-v$VERSION-x86_64-linux-gnu.tar.gz"
