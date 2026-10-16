#!/usr/bin/env bash
# Bundle a native release build as a type-2 x86_64 AppImage.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=package-common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/package-common.sh"
relay_package_init appimage "$@"

if ! $_relay_prepare_only; then
  [[ $(uname -m) == x86_64 ]] || { printf 'This package targets x86_64.\n' >&2; exit 1; }
  relay_require_tools python3 make g++ nasm ldconfig readelf install cp find xargs touch
  relay_check_toolchain
fi
relay_source_archive
cp "$_relay_root/packaging/appimage/inputs.lock" "$_relay_output/inputs.lock"
_relay_artifacts=("$_relay_archive" inputs.lock)
if $_relay_prepare_only; then
  relay_package_checksums "${_relay_artifacts[@]}"
  exit 0
fi

_relay_tools="$_relay_work/inputs"
mkdir -p "$_relay_tools"
while read -r _relay_sha _relay_file _relay_url; do
  [[ -n $_relay_sha && $_relay_sha != \#* ]] || continue
  _relay_input="$_relay_tools/$_relay_file"
  if [[ ! -f $_relay_input ]]; then
    if $_relay_offline; then
      printf 'Offline input missing: %s (run once without --offline to cache it).\n' "$_relay_input" >&2
      exit 1
    fi
    relay_require_tools curl
    curl --fail --location --retry 2 --proto '=https' --proto-redir '=https' \
      --output "$_relay_input.part" "$_relay_url"
    printf '%s  %s\n' "$_relay_sha" "$_relay_input.part" | sha256sum --check --status
    mv -- "$_relay_input.part" "$_relay_input"
  fi
  printf '%s  %s\n' "$_relay_sha" "$_relay_input" | sha256sum --check
done < "$_relay_output/inputs.lock"
chmod +x "$_relay_tools/linuxdeploy.AppImage" "$_relay_tools/appimagetool.AppImage"

_relay_source="$_relay_work/relay-$_relay_version"
_relay_appdir="$_relay_work/Relay.AppDir"
rm -rf -- "$_relay_source" "$_relay_appdir"
tar -xzf "$_relay_output/$_relay_archive" -C "$_relay_work"
cd -- "$_relay_source"
# An existing Cargo cache may be reused, including one from a distro package build.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$_relay_work/cargo}"
mkdir -p "$CARGO_TARGET_DIR"
CARGO_TARGET_DIR=$(cd -- "$CARGO_TARGET_DIR" && pwd -P)
relay_refresh_cargo_sources
cargo fetch --locked --target "$(rustc -vV | sed -n 's/^host: //p')"
cargo build --release --frozen --bin relay
cargo test --release --frozen --bin relay
relay_record_cargo_sources
desktop-file-validate packaging/common/relay.desktop
bash scripts/install-package.sh "$CARGO_TARGET_DIR/release/relay" "$_relay_appdir" /usr/share/licenses/relay

# Build the supported ABI on the same baseline as Relay. This also supplies
# software H.264 on distributions whose system OpenH264 has a different ABI.
_relay_codec="$_relay_work/openh264-2.6.0"
if [[ ! -d $_relay_codec ]]; then
  tar -xzf "$_relay_tools/openh264-2.6.0.tar.gz" -C "$_relay_work"
fi
make -C "$_relay_codec" -j "$CARGO_BUILD_JOBS" BUILDTYPE=Release ARCH=x86_64 libraries
install -Dm644 "$_relay_codec/LICENSE" "$_relay_appdir/usr/share/licenses/relay/openh264/LICENSE"

_relay_xkb=$(ldconfig -p | awk '$1 == "libxkbcommon.so.0" && /x86-64/ && !found { print $NF; found=1 }')
[[ -n $_relay_xkb ]] || { printf 'libxkbcommon.so.0 was not found by ldconfig.\n' >&2; exit 1; }
# Leave the graphics/audio platform libraries with the host, so its GPU drivers,
# Wayland connection and ALSA plugins use matching versions. linuxdeploy also
# maintains an upstream exclusion list for glibc and desktop platform libraries.
_relay_excludes=()
for _relay_pattern in 'libva.so.*' 'libva-drm.so.*' 'libva-x11.so.*' 'libvulkan.so.*' \
  'libdrm*' 'libwayland*' 'libGL*' 'libEGL*' 'libgbm*' 'libasound.so.*' '*_drv_video.so' '*_dri.so'; do
  _relay_excludes+=(--exclude-library "$_relay_pattern")
done
export APPIMAGE_EXTRACT_AND_RUN=1
"$_relay_tools/linuxdeploy.AppImage" --appdir "$_relay_appdir" \
  --executable "$_relay_appdir/usr/bin/relay" \
  --library "$_relay_codec/libopenh264.so.8" --library "$_relay_xkb" \
  --desktop-file packaging/common/relay.desktop --icon-file packaging/common/relay.svg \
  --custom-apprun packaging/appimage/AppRun "${_relay_excludes[@]}"
python3 scripts/appimage-licenses.py "$_relay_appdir"
cp "$_relay_appdir/usr/share/doc/relay/BUNDLED-LIBRARIES.txt" "$_relay_output/"

_relay_package="Relay-$_relay_version-x86_64.AppImage"
ARCH=x86_64 VERSION="$_relay_version" "$_relay_tools/appimagetool.AppImage" \
  --no-appstream --runtime-file "$_relay_tools/runtime-x86_64" \
  --mksquashfs-opt -processors --mksquashfs-opt "$CARGO_BUILD_JOBS" \
  "$_relay_appdir" "$_relay_output/$_relay_package"
chmod +x "$_relay_output/$_relay_package"
# Also works on build hosts without /dev/fuse.
"$_relay_output/$_relay_package" --appimage-extract-and-run --version
_relay_artifacts+=(BUNDLED-LIBRARIES.txt "$_relay_package")
relay_package_checksums "${_relay_artifacts[@]}"
