# Relay patch to ironrdp-pdu 0.9.0

ClearCodec RLEX (MS-RDPEGFX 2.2.4.1.1.3.1.3): with a single palette entry, the
upstream parser read every byte as a run length. Windows (and FreeRDP's decoder)
still send a packed stop-index/suite-depth byte before each run length, with a
one-bit stop index. The upstream parser therefore decoded one-colour tiles with
extra pixels and failed with "suite exceeds region pixel count", which broke the
graphics pipeline on Windows servers that send such tiles.

The patch always reads the packed byte, using `max(1, bit_length(count - 1))`
stop-index bits. Remove it when an upstream release fixes the parser.

ClearCodec short V-bar misses pack `shortVBarYOn` in the low eight bits and
`shortVBarYOff` in bits 8–13 of the little-endian header. Correct the upstream
bit extraction to match Windows and FreeRDP; the former layout rejected valid
bands with `shortVBarYOff < shortVBarYOn`.

Bitmap decoder error snapshots normalize elided lifetime names so their
diagnostics compare consistently across Rust toolchains, including Rust 1.99.
