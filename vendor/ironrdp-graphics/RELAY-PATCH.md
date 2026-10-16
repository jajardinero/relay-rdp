# Relay patches to ironrdp-graphics 0.9.0

ClearCodec reports the regions each bitmap actually draws. Relay copies those
regions onto its surfaces, preserving existing pixels when a stream contains
only bands or subcodecs. Glyph hits report the complete cached glyph.
Glyph cache hits validate the pixel count rather than the original dimensions:
Windows can reshape a cached pixel stream (for example, 2x8 into 4x4), as
specified by [MS-RDPEGFX 4.1.1.5](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/d16e42b5-30f6-4806-a00b-fa292655add6).
Rejecting these hits left updates unapplied and triggered graphics recovery.

ClearCodec's NSCodec subcodec decodes AYCoCg planes, short and extended RLE
runs, omitted planes, color loss recovery, and padded chroma subsampling
([MS-RDPNSC 3.1.8.4](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpnsc/949ee2fd-f35f-4cab-be49-e00567554177)).
The previous no-op decoder marked these regions as drawn but returned blank
pixels, producing black rectangles. Truncated planes and invalid parameters
return decode errors. `nscref.c` generates 392 synthetic FreeRDP reference
cases; application tests also exercise subregion placement and GPU uploads.

Progressive decoding follows FreeRDP's fixed-point color conversion, REGION
DWT flags, difference-tile accumulation, progressive quantization, and adaptive
SRL upgrade decoding. Output tiles retain the REGION rectangles so callers
update only the portions the server changed. The codec keeps state across
successive first and upgrade passes.
The DWT difference reference is owned by the off-screen surface, independently
of encoding context IDs. Deleting an encoding context releases refinement
state only after the last associated context is gone; it preserves the
coefficient reference until the surface is deleted/reset (MS-RDPEGFX 3.3.1.2–3).
Otherwise a zero-difference update under a new context reconstructs as gray.

Progressive tiles decode in parallel (rayon). Updates of the same tile within
one stream apply in order on one thread, and results are reported in stream
order, so output matches sequential decoding. `decode_bitmap_as` can return a
tile's DWT coefficients instead of RGBA pixels, letting Relay run the inverse
DWT and color conversion on the GPU.

Relay's application tests compare these paths with FreeRDP reference decodes
in `src/session/gfx/testdata`. Remove these patches when equivalent fixes are
available in an upstream release.
