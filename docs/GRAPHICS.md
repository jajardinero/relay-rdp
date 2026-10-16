# Graphics and video decoding

How Relay draws the remote desktop, how to tell which path a session uses, and how to
diagnose a server that doesn't send H.264. Back to the [README](../README.md).

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
