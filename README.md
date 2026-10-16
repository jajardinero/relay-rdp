# Relay

A native RDP client for Linux and Wayland, using egui and wgpu. X11 windows
and XWayland are not supported.

## Build

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
domain, display, connection speed, visual effects, audio, clipboard and
load-balancing settings, and lists any setting it can't use yet, such as RD
Gateway or RemoteApp.

Relay prefers rustls for TLS. If the TLS handshake fails because a server
requires compatibility ciphers, it retries once with OpenSSL over a fresh RDP
connection. This adds TLS 1.2 ECDHE-RSA AES-CBC/SHA-2 support; TLS 1.0 and 1.1
remain disabled. Both backends use the same certificate approval and saved
fingerprint checks before sign-in. The chosen backend, TLS version, and cipher
are recorded in Relay's logs.

Each host has a **Quality** setting under **Experience**: Best (office network),
Balanced (home broadband or VPN) or Low bandwidth (mobile or slow links). It tells
the server what kind of link you're on, so it can adjust video quality and frame
rate, and sets the wallpaper, animation and font-smoothing options to match.

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
bitmap regions into the same GPU surfaces. Everything an RDP frame draws is
recorded and submitted to the GPU once, at the end of the frame, with bounded
outstanding GPU work.

The **Sharp Text** mode, which never uses H.264, also draws on the GPU while
hardware video decoding is enabled. Its lossless codecs decode on the CPU, and
the decoded regions go straight into GPU surfaces. This needs only the Vulkan
device, not VA-API. If GPU drawing fails, Relay reconnects and draws that session
on the CPU.

RemoteFX progressive tiles decode in parallel on all CPU cores. With GPU drawing,
the CPU only entropy-decodes each tile; its wavelet transform and color
conversion run in GPU shaders, matching the CPU decoder exactly. This needs
32 KiB of compute workgroup memory, which current Vulkan drivers provide;
otherwise tiles are decoded completely on the CPU.

Stream color metadata controls conversion. When unspecified, AVC420 uses the
protocol's full-range BT.709 defaults. Filtering is clamped to the visible
picture so macroblock padding does not bleed into its edges.

If hardware setup is unavailable, Relay uses OpenH264. If an active hardware
stream fails, Relay reconnects with hardware disabled for that session, obtaining
a fresh keyframe. Persistent software graphics failures retain the existing
compatible-graphics recovery. Sharp Text and Compatible modes retain their
codec preferences; Compatible mode's classic updates always decode on the CPU.
Hardware backends other than VA-API are not implemented.

### Full color (H.264 4:4:4)

The **Full color (H.264 4:4:4)** graphics mode (host editor → **Experience**)
asks the server for AVC444, which keeps full colour detail in video regions, so
coloured text and thin lines don't blur or fringe. It uses more bandwidth and
about twice the decoding work. Windows only sends AVC444 when the group policy
**Prioritize H.264/AVC 444 graphics mode for Remote Desktop Connections** is
enabled on the server. Otherwise it sends ordinary H.264, which this mode also
handles. The other modes are unchanged.

Each AVC444 frame carries two H.264 pictures: a main view (brightness and
averaged colour) and an auxiliary view (the remaining colour samples, in the v1
or v2 layout). Both go through the surface's one decoder into a full-resolution
4:4:4 picture, which is converted to RGB. With hardware decoding, this all runs
in GPU shaders on the decoded VA-API frames; otherwise OpenH264 and the CPU do
it.

The session bar shows how the picture is rendered: a **GPU** or **CPU** badge,
followed by the codec in use (**H.264**, **H.264 4:4:4**, **Lossless** for the
graphics pipeline's other codecs, or **RemoteFX** for classic updates). The
label follows what the server actually sends, so it changes if the server falls
back to RemoteFX or sends ordinary H.264 in Full color mode.
Hover over it for details and any software fallback reason. The same information
is recorded in Relay's logs.

Every minute, and when a session ends, Relay logs a `graphics traffic` line: the
frames received; for each codec, its updates, payload size and decoding time;
and the counts of solid fills, surface copies, and cache saves and hits. It
shows what a server actually sends, and where decoding time goes.

Enabling hardware decoding advertises support; the server still chooses the
codec. If it selects V8 graphics or disables AVC, its ClearCodec/RemoteFX bitmap
updates decode on the CPU. Relay records this separately from a VA-API failure
and can still compose those bitmap updates on the GPU. A **GPU / H.264** or
**GPU / H.264 4:4:4** label confirms actual hardware video decoding. To diagnose
Windows Server or Windows 11 sessions, compare the advertised and confirmed
graphics capabilities in the logs. Check the host's **Experience → Graphics**
mode and the server's RDP encoding policies if H.264 is not selected.

For a direct Windows RDP connection, the server's
`Microsoft-Windows-RemoteDesktopServices-RdpCoreTS/Operational` event log records
the negotiated graphics capabilities (event 162) and hardware encoder selection
(event 170). This read-only PowerShell query also includes recent warnings and
errors, useful when a server disconnects during graphics negotiation:

```powershell
Get-WinEvent -FilterHashtable @{
    LogName = 'Microsoft-Windows-RemoteDesktopServices-RdpCoreTS/Operational'
    StartTime = (Get-Date).AddMinutes(-20)
} | Where-Object { $_.Id -in 162,170 -or $_.Level -le 3 } |
    Select-Object -First 12 TimeCreated,Id,Message | Format-List
```

Server encoding and client decoding are separate: Windows can encode H.264 on
its CPU while Relay decodes it on the GPU. Windows hardware encoding requires
a compatible encoder and server configuration; it is not required for Relay's
hardware decoder. See [Microsoft's encoding diagnostics](https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-enable-gpu-acceleration#verify-gpu-accelerated-frame-encoding)
and [RDP encoding policies](https://learn.microsoft.com/en-us/windows/client-management/mdm/policy-csp-admx-terminalserver#ts_server_avc444_mode_preferred).

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
snapshot includes the vendored IronRDP patches and excludes Git metadata,
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
