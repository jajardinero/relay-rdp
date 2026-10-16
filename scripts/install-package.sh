#!/usr/bin/env bash
# Install a built binary and shared assets into a package staging directory.
set -euo pipefail
[[ $# == 3 ]] || { printf 'Usage: %s BINARY DESTDIR LICENSE_DIR\n' "$0" >&2; exit 2; }
_relay_binary=$1
_relay_dest=$2
_relay_licenses=$3
_relay_source=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$_relay_source"

install -Dm755 "$_relay_binary" "$_relay_dest/usr/bin/relay"
install -Dm644 packaging/common/relay.desktop "$_relay_dest/usr/share/applications/relay.desktop"
install -Dm644 packaging/common/relay.svg "$_relay_dest/usr/share/icons/hicolor/scalable/apps/relay.svg"
install -Dm644 README.md "$_relay_dest/usr/share/doc/relay/README.md"
install -Dm644 LICENSE-MIT "$_relay_dest$_relay_licenses/LICENSE-MIT"
install -Dm644 LICENSE-APACHE "$_relay_dest$_relay_licenses/LICENSE-APACHE"
install -Dm644 assets/fonts/OFL.txt "$_relay_dest$_relay_licenses/fonts-OFL.txt"
for _relay_crate in ironrdp-connector ironrdp-egfx ironrdp-graphics ironrdp-pdu ironrdp-session sspi; do
  for _relay_file in LICENSE-MIT LICENSE-APACHE; do
    install -Dm644 "vendor/$_relay_crate/$_relay_file" \
      "$_relay_dest$_relay_licenses/$_relay_crate/$_relay_file"
  done
  if [[ -f "vendor/$_relay_crate/THIRD_PARTY_NOTICES" ]]; then
    install -Dm644 "vendor/$_relay_crate/THIRD_PARTY_NOTICES" \
      "$_relay_dest$_relay_licenses/$_relay_crate/THIRD_PARTY_NOTICES"
  fi
  if [[ -f "vendor/$_relay_crate/RELAY-PATCH.md" ]]; then
    install -Dm644 "vendor/$_relay_crate/RELAY-PATCH.md" \
      "$_relay_dest/usr/share/doc/relay/vendor/$_relay_crate/RELAY-PATCH.md"
  fi
done
