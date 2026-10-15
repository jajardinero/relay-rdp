# Relay

A native RDP client for Linux and Wayland, using egui and wgpu. X11 windows
and XWayland are not supported.

## Build

Use the Rust toolchain in `rust-toolchain.toml`, a C compiler, and `pkg-config`.
Install the system libraries and development headers for FFmpeg (`libavcodec`
and `libavutil`), VA-API (`libva` and `libva-drm`), Wayland, xkbcommon, and ALSA.
The native decoder bridge compiles against the installed FFmpeg headers; it does
not depend on handwritten Rust layouts for FFmpeg's versioned structures.

```sh
pkg-config --modversion libavcodec libavutil libva libva-drm
cargo build --release
```

OpenH264's `libopenh264.so.8` is needed for software H.264 decoding. Relay can
still use the lossless RDP codecs when OpenH264 is unavailable. Hardware decoding
needs a VA-API driver with H.264 support and a Vulkan driver that supports
DMA-BUF import, the exported DRM format modifiers, and foreign queue ownership.
The VA-API render node is matched to the Vulkan adapter, avoiding cross-GPU
frame transfers. Unsupported adapters and layouts fall back automatically.

## Usage

```sh
relay                          # host library
relay dc01.corp.example.com    # connect; host or host:port
relay --user 'CORP\jdoe' dc01  # preset the user name
relay ~/Downloads/desk.rdp     # connect with a .rdp file's settings
```

Set `RELAY_PASSWORD` to sign in without the password prompt (for scripts).
Addresses that match a saved host use its settings and saved password. A `.rdp`
file opened from the command line is used as is and is not saved. To keep one,
use **Import .rdp file** in the host library: it opens the file in the host
editor so you can review and save it. Relay reads the address, user name,
domain, display, audio, clipboard and load-balancing settings, and lists any
setting it can't use yet, such as RD Gateway or RemoteApp.

| Shortcut | Action |
| --- | --- |
| Ctrl+Alt+Enter | Toggle fullscreen |
| Ctrl+Alt+Home | Show the session bar and release system shortcuts |
| Ctrl+Alt+End | Send Ctrl+Alt+Del |

By default, system shortcuts such as Super and Alt+Tab go to the remote
computer only in fullscreen. **Settings → Keyboard** changes this.

## Video decoding

**Settings → Performance → Hardware video decoding** is enabled by default,
including for existing settings files. Changes are saved immediately and apply
to new connections and manual reconnects. Existing connections keep their
original preference, including during automatic network reconnection.

In Auto graphics mode, the accelerated path is:

```text
H.264 → FFmpeg / VA-API → DMA-BUF planes → Vulkan import
      → GPU YUV conversion → GPU surfaces / caches → egui desktop texture
```

Decoded video stays on the GPU. Solid fills, overlapping surface copies, bitmap
caches, and composition also run on the GPU. Other RDP codecs can upload decoded
bitmap regions into the same GPU surfaces. Final desktop updates are submitted
at the end of each RDP frame, with bounded outstanding GPU work.

Stream color metadata controls conversion. When unspecified, AVC420 uses the
protocol's full-range BT.709 defaults. Filtering is clamped to the visible
picture so macroblock padding does not bleed into its edges.

If hardware setup is unavailable, Relay uses OpenH264. If an active hardware
stream fails, Relay reconnects with hardware disabled for that session, obtaining
a fresh keyframe. Persistent software graphics failures retain the existing
compatible-graphics recovery. Sharp Text and Compatible modes retain their
codec preferences. AVC444 and hardware backends other than VA-API are not
implemented.

Hover over the session's statistics sparkline to see the selected decoder and
any software fallback reason. The same information is recorded in Relay's logs.

## Validation

```sh
cargo fmt --all --check
cargo check --all-targets
cargo test --all-targets
cargo test --package ironrdp-egfx --lib
cargo clippy --all-targets
```

The following tests require actual GPU access. The RDP integration test also
requires a running Wayland compositor and the built test-server example. It
launches local test servers with test credentials and uses an in-memory
certificate store; it does not modify saved hosts.

```sh
cargo build --example test_server
cargo test --bin relay -- --ignored --nocapture --test-threads=1
```

These tests compare GPU composition against a CPU reference, compare hardware
video output against software decoding at aligned and unaligned resolutions,
check descriptor cleanup across decoder resets, and exercise concurrent RDP
connections, dynamic resizing, enabled/disabled preferences, unavailable hardware,
and recovery after an injected hardware failure. A synthetic 1080p test also
reports elapsed frame time and decoding-thread CPU time for both paths.
GPU readback exists only in the test helpers, for pixel comparisons.

## Arch Linux package

From the project root, build a local x86_64 package with:

```sh
scripts/package-arch.sh
```

The helper requires Arch's `base-devel`, `pkgconf`, and `desktop-file-utils`,
Rust 1.90 or later, and the runtime dependencies listed in
`packaging/arch/PKGBUILD.in`. A working Mise-managed compiler is supported:
the helper checks all other dependencies before bypassing makepkg's check for
Arch's virtual `cargo` package. It does not install dependencies or the app.
Use `--offline` when all Cargo dependencies are already cached. Use
`--prepare-only` to generate the source archive and recipe without compiling.

The build uses the locked dependency versions, runs the normal tests in release
mode, and validates the desktop entry. Hardware tests remain opt-in. Its source
snapshot includes both vendored IronRDP patches and excludes Git metadata,
private configuration, build caches, and generated packages.
The helper defaults to two compilation jobs to bound release-build memory use;
set `CARGO_BUILD_JOBS` to override this.

The resulting `dist/arch` directory contains a `.pkg.tar.zst`, a self-contained
source archive, a generated `PKGBUILD`, `.SRCINFO`, and `SHA256SUMS`. Keep the
PKGBUILD and source archive together to rebuild with `makepkg` on an Arch system
with the listed dependencies installed. Build intermediates stay under
`target/arch-package`. These generated directories are ignored by Git.

For version 0.1.0, verify and install with:

```sh
(cd dist/arch && sha256sum --check SHA256SUMS)
sudo pacman -U dist/arch/relay-0.1.0-1-x86_64.pkg.tar.zst
```

The package installs `/usr/bin/relay` and a **Relay** application-menu entry with
an SVG icon. It includes project, font, and vendored license notices, and uses
system FFmpeg/VA-API libraries. OpenH264 is a required dependency so software
fallback is available. The package records the compiled FFmpeg library ABIs and
the OpenH264 `.so.8` ABI; rebuild it after incompatible library upgrades.

A Wayland compositor is required. Install a Vulkan driver and a VA-API H.264
driver appropriate for your GPU to use hardware decoding. A Secret Service
provider is optional for saving passwords. The package does not register file
or URI associations, change default applications, or alter desktop settings.
