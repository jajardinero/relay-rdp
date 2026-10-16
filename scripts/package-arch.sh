#!/usr/bin/env bash
# Build local Arch packages without installing packages or changing host config.
set -euo pipefail

_relay_offline=false
_relay_prepare_only=false
for _relay_arg in "$@"; do
  case "$_relay_arg" in
    --offline) _relay_offline=true ;;
    --prepare-only) _relay_prepare_only=true ;;
    -h|--help)
      printf 'Usage: %s [--offline] [--prepare-only]\n\n' "$0"
      printf '%s\n' \
        'Build a local x86_64 Arch package in dist/arch, without installing it.' \
        '--offline       Use only dependencies already cached by Cargo.' \
        '--prepare-only  Generate the source archive, PKGBUILD, and .SRCINFO.'
      exit 0 ;;
    *) printf 'Unknown option: %s\n' "$_relay_arg" >&2; exit 2 ;;
  esac
done

_relay_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
_relay_output="$_relay_root/dist/arch"
_relay_work="$_relay_root/target/arch-package"
for _relay_command in cargo rustc makepkg pacman pkg-config tar gzip sha256sum awk sed sort; do
  command -v "$_relay_command" >/dev/null || {
    printf 'Required build tool is missing: %s\n' "$_relay_command" >&2
    exit 1
  }
done
[[ $(uname -m) == x86_64 ]] || { printf 'This package targets x86_64.\n' >&2; exit 1; }
[[ $EUID != 0 ]] || { printf 'Run makepkg as your normal user, not root.\n' >&2; exit 1; }

# Read only the package section, not dependency or release-profile versions.
_relay_version=$(awk '
  /^\[package\]$/ { package_section=1; next }
  package_section && /^\[/ { exit }
  package_section && /^version[[:space:]]*=/ { split($0, parts, "\""); print parts[2]; exit }
' "$_relay_root/Cargo.toml")
_relay_min_rust=$(awk '
  /^rust-version[[:space:]]*=/ { split($0, parts, "\""); print parts[2]; exit }
' "$_relay_root/Cargo.toml")
[[ $_relay_version =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+][[:alnum:].]+)?$ ]] || {
  printf 'Unsupported Arch package version in Cargo.toml: %s\n' "$_relay_version" >&2
  exit 1
}
_relay_rust=$(rustc --version | awk '{print $2}')
[[ $(printf '%s\n' "$_relay_min_rust" "$_relay_rust" | sort -V | sed -n '1p') == "$_relay_min_rust" ]] || {
  printf 'Rust %s or later is required; found %s.\n' "$_relay_min_rust" "$_relay_rust" >&2
  exit 1
}

mkdir -p -- "$_relay_output" "$_relay_work"
_relay_archive="relay-$_relay_version.tar.gz"
_relay_sources=(
  Cargo.toml Cargo.lock build.rs rust-toolchain.toml
  src assets examples vendor README.md LICENSE-MIT LICENSE-APACHE
  packaging scripts
)
# Normalize ownership and timestamps without exporting an artificial package
# build date. Only explicitly listed project sources enter the archive.
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner --exclude=__pycache__ \
  --transform="s|^|relay-$_relay_version/|" \
  -C "$_relay_root" -cf - "${_relay_sources[@]}" | gzip -n > "$_relay_output/$_relay_archive"
_relay_digest=$(sha256sum "$_relay_output/$_relay_archive" | awk '{print $1}')
sed -e "s/@VERSION@/$_relay_version/g" -e "s/@SOURCE_SHA256@/$_relay_digest/g" \
  "$_relay_root/packaging/arch/PKGBUILD.in" > "$_relay_output/PKGBUILD"

export PKGDEST="$_relay_output"
export SRCDEST="$_relay_output"
export SRCPKGDEST="$_relay_output"
export BUILDDIR="$_relay_work/build"
mkdir -p -- "$BUILDDIR"
cd -- "$_relay_output"
# Source our own generated recipe to inspect the exact dependency lists and
# avoid skipping runtime dependencies when a compiler is managed outside pacman.
# shellcheck source=/dev/null
source ./PKGBUILD
makepkg --printsrcinfo > .SRCINFO
# --force: a rebuild of the same version replaces the previous package.
makepkg --verifysource --force

_relay_makepkg_flags=(--force --cleanbuild --check --nosign)
if ! pacman -T cargo >/dev/null; then
  # Mise/rustup can provide working tools without registering Arch's virtual
  # cargo package. Validate every other dependency before using --nodeps.
  # Dependency arrays come from the generated PKGBUILD above.
  # shellcheck disable=SC2154
  pacman -T "${depends[@]}" "${checkdepends[@]}" pkgconf base-devel || {
    printf 'Install the missing Arch dependencies above before building.\n' >&2
    exit 1
  }
  printf 'Using existing external toolchain: %s\n' "$(cargo --version)"
  _relay_makepkg_flags+=(--nodeps)
fi

if ! $_relay_prepare_only; then
  if $_relay_offline; then export CARGO_NET_OFFLINE=true; fi
  export CARGO_TARGET_DIR="$_relay_work/cargo"
  # Release LTO is memory intensive; callers can raise this on larger machines.
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
  makepkg "${_relay_makepkg_flags[@]}"
fi

_relay_artifacts=(PKGBUILD .SRCINFO "$_relay_archive")
if ! $_relay_prepare_only; then
  while IFS= read -r _relay_package; do
    _relay_artifacts+=("${_relay_package##*/}")
  done < <(makepkg --packagelist)
fi
sha256sum "${_relay_artifacts[@]}" > SHA256SUMS
printf '\nArtifacts: %s\n' "$_relay_output"
if ! $_relay_prepare_only; then
  printf 'Package: %s\n' "${_relay_artifacts[3]}"
fi
