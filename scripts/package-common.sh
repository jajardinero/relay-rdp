#!/usr/bin/env bash
# Shared helpers for the local package builders.

relay_package_init() {
  local format=$1 arg
  shift
  _relay_offline=false
  _relay_prepare_only=false
  for arg in "$@"; do
    case "$arg" in
      --offline) _relay_offline=true ;;
      --prepare-only) _relay_prepare_only=true ;;
      -h|--help)
        printf 'Usage: %s [--offline] [--prepare-only]\n\n' "$0"
        printf 'Build a local x86_64 %s package in dist/%s, without installing it.\n' "$format" "$format"
        printf '%s\n' \
          '--offline       Use only cached Cargo dependencies and packaging inputs.' \
          '--prepare-only  Generate sources and packaging metadata on any Linux distro.'
        exit 0 ;;
      *) printf 'Unknown option: %s\n' "$arg" >&2; exit 2 ;;
    esac
  done
  _relay_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
  _relay_output="$_relay_root/dist/$format"
  _relay_work="$_relay_root/target/$format-package"
  relay_require_tools awk sed sort tar gzip sha256sum
  _relay_version=$(awk '
    /^\[package\]$/ { package_section=1; next }
    package_section && /^\[/ { exit }
    package_section && /^version[[:space:]]*=/ { split($0, parts, "\""); print parts[2]; exit }
  ' "$_relay_root/Cargo.toml")
  _relay_min_rust=$(awk '
    /^rust-version[[:space:]]*=/ { split($0, parts, "\""); print parts[2]; exit }
  ' "$_relay_root/Cargo.toml")
  [[ $_relay_version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    printf 'Unsupported package version in Cargo.toml: %s\n' "$_relay_version" >&2
    exit 1
  }
}

relay_require_tools() {
  local command
  for command in "$@"; do
    command -v "$command" >/dev/null || {
      printf 'Required build tool is missing: %s\n' "$command" >&2
      exit 1
    }
  done
}

relay_check_toolchain() {
  relay_require_tools cargo rustc cc pkg-config desktop-file-validate
  local version
  version=$(rustc --version | awk '{print $2}')
  [[ $(printf '%s\n' "$_relay_min_rust" "$version" | sort -V | sed -n '1p') == "$_relay_min_rust" ]] || {
    printf 'Rust %s or later is required; found %s.\n' "$_relay_min_rust" "$version" >&2
    exit 1
  }
  pkg-config --exists libavcodec libavutil libva libva-drm wayland-client xkbcommon alsa openssl || {
    printf 'Install the system development libraries listed in README.md before building.\n' >&2
    exit 1
  }
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
  if $_relay_offline; then export CARGO_NET_OFFLINE=true; fi
}

relay_source_archive() {
  mkdir -p -- "$_relay_output" "$_relay_work"
  _relay_archive="relay-$_relay_version.tar.gz"
  # Allowlist project sources; never include local config, Git metadata or build caches.
  tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
    --exclude=.git --exclude=.agents --exclude=.codex --exclude=.aws --exclude=target --exclude=__pycache__ \
    --transform="s|^|relay-$_relay_version/|" \
    -C "$_relay_root" -cf - \
    Cargo.toml Cargo.lock build.rs rust-toolchain.toml \
    src assets examples vendor README.md LICENSE-MIT LICENSE-APACHE packaging scripts \
    | gzip -n > "$_relay_output/$_relay_archive"
}

relay_cargo_sources_key() {
  # Archive timestamps are normalized. Cargo's mtime-based cache must also be
  # invalidated when source bytes change without a package-version change.
  local digest
  digest=$(
    # The caller sets _relay_source to its extracted build tree.
    # shellcheck disable=SC2154
    (cd -- "$_relay_source" && {
      find src assets vendor -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum
      sha256sum Cargo.toml Cargo.lock build.rs rust-toolchain.toml
    }) | sha256sum | awk '{print $1}'
  )
  printf '%s %s\n' "$digest" "$_relay_source"
}

relay_refresh_cargo_sources() {
  local key stamp mtime
  key=$(relay_cargo_sources_key)
  stamp="$CARGO_TARGET_DIR/.relay-package-sources"
  if [[ -f $stamp ]] && [[ $(sed -n '1p' "$stamp") == "$key" ]]; then
    mtime=$(sed -n '2p' "$stamp")
  else
    mtime=''
  fi
  mtime=${mtime:-$(date -u '+%Y-%m-%d %H:%M:%S.%N %z')}
  # Restore the same mtime for unchanged inputs, including build-script inputs.
  find "$_relay_source" -type f -exec touch -d "$mtime" {} +
}

relay_record_cargo_sources() {
  # Record only after the release build and tests have both succeeded.
  { relay_cargo_sources_key; stat -c %y "$_relay_source/build.rs"; } \
    > "$CARGO_TARGET_DIR/.relay-package-sources"
}

relay_package_checksums() {
  (cd -- "$_relay_output" && sha256sum "$@" > SHA256SUMS)
  printf '\nArtifacts: %s\n' "$_relay_output"
}
