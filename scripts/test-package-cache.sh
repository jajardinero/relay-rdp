#!/usr/bin/env bash
# Regression test: normalized snapshots must rebuild changed code and assets.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=package-common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/package-common.sh"
_relay_project=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
mkdir -p "$_relay_project/target"
_relay_fixture=$(mktemp -d "$_relay_project/target/package-cache-test.XXXXXX")
trap 'rm -rf -- "$_relay_fixture"' EXIT
_relay_source="$_relay_fixture/source"
export CARGO_TARGET_DIR="$_relay_fixture/cargo"
mkdir -p "$_relay_source/"{src,assets,vendor} "$CARGO_TARGET_DIR"
cd -- "$_relay_source"
cat > Cargo.toml <<'TOML'
[package]
name = "relay-cache-test"
version = "0.1.0"
edition = "2024"
TOML
printf 'fn main() {}\n' > build.rs
printf '[toolchain]\nchannel = "stable"\n' > rust-toolchain.toml
printf 'fn main() { println!("{}", include_str!("../assets/message.txt").trim()); }\n' > src/main.rs
printf 'before\n' > assets/message.txt
cargo generate-lockfile --offline

build_snapshot() {
  # Match the normalized timestamps in generated source archives.
  find "$_relay_source" -type f -exec touch -d @0 {} +
  relay_refresh_cargo_sources
  cargo build --release --locked --offline --quiet
  relay_record_cargo_sources
}

build_snapshot
[[ $("$CARGO_TARGET_DIR/release/relay-cache-test") == before ]]
printf 'asset-change\n' > assets/message.txt
build_snapshot
[[ $("$CARGO_TARGET_DIR/release/relay-cache-test") == asset-change ]]
printf 'fn main() { println!("code-change"); }\n' > src/main.rs
build_snapshot
[[ $("$CARGO_TARGET_DIR/release/relay-cache-test") == code-change ]]
_relay_built_at=$(stat -c %y "$CARGO_TARGET_DIR/release/relay-cache-test")
build_snapshot
[[ $(stat -c %y "$CARGO_TARGET_DIR/release/relay-cache-test") == "$_relay_built_at" ]]
printf 'Packaging cache: changed assets/code rebuilt; unchanged snapshot reused.\n'
