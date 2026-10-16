# Building and packaging Relay

Back to the [README](../README.md).

## Build from source

Use the Rust toolchain in `rust-toolchain.toml`, a C compiler, and `pkg-config`.
Install the system libraries and development headers for FFmpeg (`libavcodec`
and `libavutil`), VA-API (`libva` and `libva-drm`), Wayland, xkbcommon, ALSA,
and OpenSSL.
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

## Validation

```sh
cargo fmt --all --check
cargo check --all-targets
cargo test --all-targets
cargo test --package ironrdp-egfx --lib
cargo clippy --all-targets
bash scripts/test-package-cache.sh
```

The following tests require actual GPU access. The RDP integration test also
requires a running Wayland compositor and the built test-server example. It
launches local test servers with test credentials and uses an in-memory
certificate store; it does not modify saved hosts.

```sh
cargo build --example test_server
cargo test --bin relay -- --ignored --nocapture --test-threads=1
```

To try a session by hand, run the local test server (user and password `test`)
with `--gfx` for H.264 or `--gfx444` for H.264 4:4:4, and connect to
`127.0.0.1:3390`:

```sh
cargo run --example test_server -- --gfx444
```

These tests compare GPU composition against a CPU reference, compare hardware
video output (4:2:0 and 4:4:4) against software decoding at aligned and
unaligned resolutions, stream 4:4:4 to hardware and software sessions,
check descriptor cleanup across decoder resets, and exercise concurrent RDP
connections, dynamic resizing, enabled/disabled preferences, unavailable hardware,
and recovery after an injected hardware failure. A synthetic 1080p test also
reports elapsed frame time and decoding-thread CPU time for both paths.
GPU readback exists only in the test helpers, for pixel comparisons.

## Linux packages

Local package builders are available for these distributions on x86_64:

| Distribution | Build command | Output |
| --- | --- | --- |
| Debian / Ubuntu | `scripts/package-deb.sh` | `dist/deb/*.deb` |
| Fedora | `scripts/package-fedora.sh` | `dist/fedora/*.rpm` (binary and source RPMs) |
| Arch Linux | `scripts/package-arch.sh` | `dist/arch/*.pkg.tar.zst` |
| AppImage | `scripts/package-appimage.sh` | `dist/appimage/*.AppImage` |

Run each native package builder on the destination distribution and release (a container or VM
works). The packages link to that distribution's FFmpeg, OpenSSL and other
system libraries; an Arch-built binary cannot be repackaged for Debian or Fedora.
Build a separate package for each distribution/release you intend to distribute.

All builders use the locked Cargo dependencies, run the normal tests in release
mode, validate the desktop entry, and include the launcher, SVG icon, README,
font license and vendored license notices. They leave the resulting packages in
`dist/` and build intermediates in `target/`, without installing the application
or dependencies. Compilation defaults to two jobs; set `CARGO_BUILD_JOBS` to
override it. Hardware tests remain opt-in.

The helpers accept `--offline` when Cargo dependencies are already cached and
`--prepare-only` to generate the source archive and packaging metadata without
compiling. Debian, Fedora and AppImage preparation works on any Linux distribution and
needs only Bash and the usual GNU file/archive utilities. The source archives
include the packaging scripts and patched vendored crates, and exclude Git
metadata, private configuration and build caches. Extract an archive and run its
builder to rebuild it. Each output directory includes `SHA256SUMS` for that run's
artifacts.

### AppImage

Build on the oldest distribution you want to support. Ubuntu 24.04 is a suitable
baseline for current Ubuntu, Debian and Fedora desktops. Building on a newer
system raises the AppImage's glibc requirement; AppImage does not bundle glibc.
See the [AppImage packaging guide](https://docs.appimage.org/packaging-guide/from-source/native-binaries.html).

Install the normal native build dependencies listed below, Rust 1.95 or later,
and these extra packaging tools (Ubuntu/Debian example):

```sh
sudo apt install curl python3 nasm make g++
scripts/package-appimage.sh
```

The builder downloads fixed, SHA-256-verified releases of linuxdeploy,
appimagetool, the AppImage runtime, and OpenH264 source. It builds OpenH264 2.6.0
alongside Relay, so software H.264 also works on hosts with an older codec ABI.
The input versions, URLs and hashes are in `packaging/appimage/inputs.lock`.
Inputs are cached under `target/appimage-package/inputs`; `--offline` needs that
cache as well as the Cargo cache. `--prepare-only` needs neither and does not
download or compile anything. `CARGO_TARGET_DIR` can select an existing build
cache on the same build system.

linuxdeploy bundles FFmpeg, OpenSSL, OpenH264 and their application libraries,
with relative library paths. The image includes distro library copyright and
license notices plus a `BUNDLED-LIBRARIES.txt` inventory, also copied to `dist/appimage`.
License collection supports Debian/Ubuntu, Arch and Fedora build hosts.

The host supplies Wayland, ALSA, VA-API, the Vulkan loader and GPU drivers. A
Wayland desktop with those platform libraries is required; hardware H.264 still
needs a suitable VA-API driver and Vulkan support. These platform libraries are
kept outside the bundle so the host's drivers and audio plugins use matching
versions. System certificate stores and desktop portals are used normally.

```sh
(cd dist/appimage && sha256sum --check SHA256SUMS)
chmod +x dist/appimage/Relay-0.1.2-x86_64.AppImage
./dist/appimage/Relay-0.1.2-x86_64.AppImage
```

On a system without usable FUSE support, run it with
`--appimage-extract-and-run` before Relay's arguments, or use
`--appimage-extract` and launch `squashfs-root/AppRun`. The build itself uses
extraction and does not need `/dev/fuse`.

### Debian and Ubuntu

Install the native build dependencies:

```sh
sudo apt install build-essential pkg-config dpkg-dev desktop-file-utils \
  libavcodec-dev libavutil-dev libva-dev libwayland-dev libxkbcommon-dev \
  libasound2-dev libssl-dev
```

Provide Rust 1.95 or later (for example, a current stable rustup toolchain), then
build and install:

```sh
scripts/package-deb.sh
(cd dist/deb && sha256sum --check SHA256SUMS)
sudo apt install ./dist/deb/relay_0.1.2-1_amd64.deb
```

The builder derives shared-library dependencies with `dpkg-shlibdeps`, including
the FFmpeg ABIs on the build system. Libraries loaded at runtime, including
Wayland and Vulkan, are listed explicitly. The package recommends
`libopenh264-8` for software H.264 decoding and suggests `fuse3` for clipboard
file transfers and `gnome-keyring` for saved passwords.

Debian 13 and Ubuntu 26.04 provide the supported OpenH264 ABI. Ubuntu 24.04 ships
`libopenh264-7`, which Relay cannot load; the `.deb` can still be built and used
with hardware decoding or lossless graphics, but software H.264 needs
`libopenh264.so.8`. See the [Debian package](https://packages.debian.org/trixie/libopenh264-8)
and [Ubuntu package versions](https://packages.ubuntu.com/search?keywords=openh264&searchon=names).

### Fedora

Install the native build dependencies:

```sh
sudo dnf install rust cargo gcc gcc-c++ rpm-build redhat-rpm-config pkgconf-pkg-config \
  desktop-file-utils libavcodec-free-devel libavutil-free-devel libva-devel \
  wayland-devel libxkbcommon-devel alsa-lib-devel openssl-devel
```

The Rust and Cargo packages must be version 1.95 or later. If you use a full
FFmpeg installation instead of Fedora's `ffmpeg-free` libraries, use its matching
development package (such as `ffmpeg-devel`) instead of the two `-free-devel`
packages. The spec uses `pkgconfig` requirements to accept either provider.

```sh
scripts/package-fedora.sh
(cd dist/fedora && sha256sum --check SHA256SUMS)
sudo dnf install ./dist/fedora/relay-0.1.2-1.fc*.x86_64.rpm
```

RPM records the linked library ABIs automatically. Runtime-loaded libraries are
listed explicitly. The package recommends the real `openh264` codec providing
`libopenh264.so.8` from Fedora's Cisco repository; the `noopenh264` stub cannot decode
video. See [Fedora's OpenH264 documentation](https://fedoraproject.org/wiki/OpenH264).
Hardware decoding also needs an FFmpeg build with an H.264 decoder and a VA-API
driver with H.264 support.

The generated `relay.spec` and source archive can be used directly with
`rpmbuild -ba`: set RPM's `_sourcedir` to the directory containing the archive.
The spec fetches locked Cargo dependencies before building, so this local
recipe needs network access or a populated Cargo cache. It is not a Fedora
repository submission with all Cargo sources vendored for offline builders.

### Arch Linux

From the project root, build a local x86_64 package with:

```sh
scripts/package-arch.sh
```

The helper requires Arch's `base-devel`, `pkgconf`, and `desktop-file-utils`,
Rust 1.95 or later, and the runtime dependencies listed in
`packaging/arch/PKGBUILD.in`. A working Mise-managed compiler is supported:
the helper checks all other dependencies before bypassing makepkg's check for
Arch's virtual `cargo` package. It does not install dependencies or the app.
Use `--offline` when all Cargo dependencies are already cached. Use
`--prepare-only` to generate the source archive and recipe without compiling.

The build uses the locked dependency versions, runs the normal tests in release
mode, and validates the desktop entry. Hardware tests remain opt-in. Its source
snapshot includes the vendored IronRDP patches and excludes Git metadata,
private configuration, build caches, and generated packages.
The helper defaults to two compilation jobs to bound release-build memory use;
set `CARGO_BUILD_JOBS` to override this.

The resulting `dist/arch` directory contains a `.pkg.tar.zst`, a self-contained
source archive, a generated `PKGBUILD`, `.SRCINFO`, and `SHA256SUMS`. Keep the
PKGBUILD and source archive together to rebuild with `makepkg` on an Arch system
with the listed dependencies installed. Build intermediates stay under
`target/arch-package`. These generated directories are ignored by Git.

For version 0.1.2, verify and install with:

```sh
(cd dist/arch && sha256sum --check SHA256SUMS)
sudo pacman -U dist/arch/relay-0.1.2-1-x86_64.pkg.tar.zst
```

The package installs `/usr/bin/relay` and a **Relay** application-menu entry with
an SVG icon. It includes project, font, and vendored license notices, and uses
system FFmpeg/VA-API libraries. OpenH264 is a required dependency so software
fallback is available. The package records the compiled FFmpeg library ABIs and
the OpenH264 `.so.8` ABI; rebuild it after incompatible library upgrades.

A Wayland compositor is required. Install a Vulkan driver and a VA-API H.264
driver appropriate for your GPU to use hardware decoding. A Secret Service
provider is optional for saving passwords, and `fuse3` for pasting files copied
on the remote computer. The package does not register file
or URI associations, change default applications, or alter desktop settings.
