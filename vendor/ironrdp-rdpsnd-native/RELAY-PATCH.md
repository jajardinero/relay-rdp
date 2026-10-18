# Relay patch to ironrdp-rdpsnd-native 0.7.0

Audio playback no longer blocks the real-time audio thread (`RxBuffer` in
`src/cpal.rs`). Upstream, the device callback waited up to 4 s for the next
packet whenever none was queued, and on timeout left the rest of the device
buffer unfilled. Every network hiccup (or a pause in the server's audio)
stalled the audio thread, so PipeWire reported "Buffer underrun/overrun
occurred" and playback stuttered; the 4 s timeouts were logged as warnings.

The callback now only takes what has already arrived and plays silence when
it runs dry. Playback starts (and restarts after running dry) once 80 ms are
queued, which absorbs network jitter, and beyond 300 ms the oldest audio is
dropped on frame boundaries so playback never drifts far behind the picture.
