#!/usr/bin/env bash
# Build a local Debian/Ubuntu package against the build distro's library ABIs.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=package-common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/package-common.sh"
relay_package_init deb "$@"

if ! $_relay_prepare_only; then
  [[ -f /etc/debian_version ]] || {
    printf 'Build .deb packages on Debian/Ubuntu; use --prepare-only on other distros.\n' >&2
    exit 1
  }
  relay_require_tools dpkg-deb dpkg-shlibdeps dpkg-architecture strip du find xargs md5sum install touch
  [[ $(dpkg-architecture -qDEB_HOST_ARCH) == amd64 ]] || {
    printf 'This package targets amd64.\n' >&2; exit 1
  }
  relay_check_toolchain
fi

relay_source_archive
sed "s/@VERSION@/$_relay_version/g" "$_relay_root/packaging/debian/control.in" \
  > "$_relay_output/control.in"
_relay_artifacts=("$_relay_archive" control.in)

if ! $_relay_prepare_only; then
  # Build from the same source snapshot that is distributed with the package.
  _relay_source="$_relay_work/relay-$_relay_version"
  rm -rf -- "$_relay_source" "$_relay_work/stage"
  tar -xzf "$_relay_output/$_relay_archive" -C "$_relay_work"
  cd -- "$_relay_source"
  export CARGO_TARGET_DIR="$_relay_work/cargo"
  relay_refresh_cargo_sources
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/^host: //p')"
  cargo build --release --frozen --bin relay
  cargo test --release --frozen --bin relay
  relay_record_cargo_sources
  desktop-file-validate packaging/common/relay.desktop
  _relay_stage="$_relay_work/stage"
  bash scripts/install-package.sh "$CARGO_TARGET_DIR/release/relay" "$_relay_stage" /usr/share/doc/relay
  strip --strip-unneeded "$_relay_stage/usr/bin/relay"
  install -Dm644 packaging/debian/copyright "$_relay_stage/usr/share/doc/relay/copyright"
  # dpkg-shlibdeps needs source-package context even when building via dpkg-deb.
  mkdir -p debian "$_relay_stage/DEBIAN"
  printf 'Source: relay\n\nPackage: relay\nArchitecture: amd64\n' > debian/control
  _relay_depends=$(dpkg-shlibdeps -O -e"$_relay_stage/usr/bin/relay" | sed -n 's/^shlibs:Depends=//p')
  [[ -n $_relay_depends ]] || { printf 'No shared-library dependencies found.\n' >&2; exit 1; }
  _relay_size=$(du -sk "$_relay_stage/usr" | awk '{print $1}')
  sed -e "s/@SHLIBS@/$_relay_depends/" -e "s/@INSTALLED_SIZE@/$_relay_size/" \
    "$_relay_output/control.in" > "$_relay_stage/DEBIAN/control"
  (cd -- "$_relay_stage" && find usr -type f -print0 | sort -z | xargs -0 md5sum > DEBIAN/md5sums)
  _relay_package="relay_${_relay_version}-1_amd64.deb"
  dpkg-deb --build --root-owner-group "$_relay_stage" "$_relay_output/$_relay_package"
  _relay_artifacts+=("$_relay_package")
fi

relay_package_checksums "${_relay_artifacts[@]}"
