# Relay patch to ironrdp-egfx 0.3.0

The handler can opt into external AVC420 processing through `handles_avc420`
and `on_avc420`. The client forwards the decoded stream structure and original
destination rectangle without allocating CPU RGBA data. Such handlers count as
H.264-capable during capability advertisement. All default handlers preserve
the upstream decoder and no-decoder behavior. Existing surface/reset callbacks
and frame acknowledgements remain in use.

This supports Relay's per-surface VA-API decoders and GPU compositor. Remove the
patch when an upstream release provides an equivalent external-frame API.
