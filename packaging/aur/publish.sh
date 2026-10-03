#!/usr/bin/env bash
# Publish PKGBUILDs to the AUR, normally inside archlinux:base-devel.
# VERSION and GITHUB_REPOSITORY are required; an unset AUR_SSH_PRIVATE_KEY
# skips publication. AUR_USERNAME/AUR_EMAIL select the commit identity.
set -euo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

sha256_of() {
  curl -fsSL "$1" | sha256sum | cut -d' ' -f1
}

cleanup_workdir() {
  local status=$? attempt
  trap - EXIT
  for attempt in 1 2 3; do
    if rm -rf -- "$WORK_DIR"; then
      exit "$status"
    fi
  done
  echo "::error::could not clean temporary publish directory $WORK_DIR" >&2
  exit 1
}

prepare_workdir() {
  WORK_DIR="$(mktemp -d)"
  trap cleanup_workdir EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  if [ "$(id -u)" -eq 0 ]; then
    id builder >/dev/null 2>&1 || useradd -m builder
    AS_USER=(runuser -u builder --)
    # The builder must traverse this root-owned parent to reach its checkout.
    chmod 711 "$WORK_DIR"
  else
    AS_USER=()
  fi
}

publish() {
  local pkg="$1" template="$2" src_url="$3" dir checksum diff_status
  dir="$WORK_DIR/$pkg"

  echo "=== $pkg $VERSION ==="
  if ! git clone --depth 1 "ssh://aur@aur.archlinux.org/$pkg.git" "$dir" 2>/dev/null; then
    mkdir -p "$dir"
    git -C "$dir" init -b master
    git -C "$dir" remote add origin "ssh://aur@aur.archlinux.org/$pkg.git"
  fi

  # A standalone assignment propagates curl/pipefail's status. Substituting
  # directly inside sed would publish an empty-download hash after failure.
  checksum="$(sha256_of "$src_url")"
  if [[ ! "$checksum" =~ ^[0-9a-f]{64}$ ]]; then
    echo "::error::invalid source checksum for $pkg" >&2
    exit 1
  fi
  cp "$template" "$dir/PKGBUILD"
  sed -i \
    -e "s/^pkgver=.*/pkgver=$VERSION/" \
    -e "s|^sha256sums=.*|sha256sums=('$checksum')|" \
    "$dir/PKGBUILD"

  if [ "${#AS_USER[@]}" -gt 0 ]; then
    # makepkg checks writable build/source/package dirs even for --printsrcinfo.
    chown -R builder "$dir"
  fi
  "${AS_USER[@]}" sh -c 'cd "$1" && makepkg --printsrcinfo' -- "$dir" > "$dir/.SRCINFO"
  # Only this owned temporary checkout is trusted across the ownership change;
  # root still performs SSH operations using its configured AUR key.
  git -c "safe.directory=$dir" -C "$dir" add PKGBUILD .SRCINFO
  if git -c "safe.directory=$dir" -C "$dir" diff --cached --quiet; then
    echo "$pkg is already up to date"
    return
  else
    diff_status=$?
    if [ "$diff_status" -ne 1 ]; then
      echo "::error::could not check staged changes for $pkg" >&2
      return "$diff_status"
    fi
  fi
  git -c "safe.directory=$dir" -C "$dir" commit -m "v$VERSION"
  git -c "safe.directory=$dir" -C "$dir" push origin HEAD:master
  echo "published $pkg $VERSION"
}

main() {
  VERSION="${VERSION:?set VERSION}"
  VERSION="${VERSION#v}"
  REPO="${GITHUB_REPOSITORY:?set GITHUB_REPOSITORY}"
  if [ -z "${AUR_SSH_PRIVATE_KEY:-}" ]; then
    echo "::notice::AUR_SSH_PRIVATE_KEY not set — skipping AUR publish"
    return
  fi

  install -d -m 700 ~/.ssh
  printf '%s\n' "$AUR_SSH_PRIVATE_KEY" > ~/.ssh/aur
  chmod 600 ~/.ssh/aur
  ssh-keyscan -t ed25519,rsa aur.archlinux.org >> ~/.ssh/known_hosts 2>/dev/null
  export GIT_SSH_COMMAND="ssh -i ~/.ssh/aur -o IdentitiesOnly=yes"
  git config --global user.name "${AUR_USERNAME:-aur-publish[bot]}"
  git config --global user.email "${AUR_EMAIL:-aur-publish@localhost}"

  prepare_workdir
  publish dictationapp \
    "$SELF_DIR/PKGBUILD" \
    "https://github.com/$REPO/archive/refs/tags/v$VERSION.tar.gz"
  publish dictationapp-bin \
    "$SELF_DIR/PKGBUILD-bin" \
    "https://github.com/$REPO/releases/download/v$VERSION/dictationapp-v$VERSION-x86_64-linux-gnu.tar.gz"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
