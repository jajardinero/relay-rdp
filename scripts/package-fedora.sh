#!/usr/bin/env bash
# Build local Fedora binary and source RPMs without installing packages.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=package-common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/package-common.sh"
relay_package_init fedora "$@"

if ! $_relay_prepare_only; then
  [[ -f /etc/fedora-release ]] || {
    printf 'Build Fedora RPMs on Fedora; use --prepare-only on other distros.\n' >&2
    exit 1
  }
  relay_require_tools rpmbuild rpm find cp
  [[ $(rpm --eval '%{_arch}') == x86_64 ]] || {
    printf 'This package targets x86_64.\n' >&2; exit 1
  }
  relay_check_toolchain
fi

relay_source_archive
sed -e "s/@VERSION@/$_relay_version/g" -e "s/@MIN_RUST@/$_relay_min_rust/g" \
  "$_relay_root/packaging/fedora/relay.spec.in" > "$_relay_output/relay.spec"
_relay_artifacts=("$_relay_archive" relay.spec)

if ! $_relay_prepare_only; then
  # Keep rpmbuild's build roots, caches and outputs inside this project's target/.
  mkdir -p "$_relay_work/rpmbuild" "$_relay_work/cargo"
  export CARGO_TARGET_DIR="$_relay_work/cargo"
  rpmbuild -ba "$_relay_output/relay.spec" \
    --define "_topdir $_relay_work/rpmbuild" \
    --define "_sourcedir $_relay_output"
  while IFS= read -r -d '' _relay_package; do
    cp -- "$_relay_package" "$_relay_output/"
    _relay_artifacts+=("${_relay_package##*/}")
  done < <(find "$_relay_work/rpmbuild/RPMS" "$_relay_work/rpmbuild/SRPMS" \
    -type f -name "relay-$_relay_version-*.rpm" -print0 | sort -z)
  [[ ${#_relay_artifacts[@]} -gt 2 ]] || { printf 'rpmbuild produced no packages.\n' >&2; exit 1; }
fi

relay_package_checksums "${_relay_artifacts[@]}"
