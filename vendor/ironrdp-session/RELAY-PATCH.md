# Relay patch to ironrdp-session 0.11.0

Bitmap updates are cut to their destination rectangle before they are drawn
(`clip_to_rectangle` in `src/fast_path.rs`). A TS_BITMAP_DATA's pixels are
`width` by `height`, but its destination can be smaller: Windows Server 2008
sends 64x64 tiles cut at the desktop's edges, and widths are rounded up to a
multiple of four. The image functions step through the pixels by the
destination's width and copy every row, so tiles at the bottom edge wrote past
the end of the framebuffer (a panic) and narrower tiles were drawn sheared.
