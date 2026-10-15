fn main() {
    println!("cargo:rerun-if-changed=src/session/gfx/vaapi.c");
    let mut build = cc::Build::new();
    for name in ["libavcodec", "libavutil", "libva", "libva-drm"] {
        let lib = pkg_config::Config::new().probe(name).unwrap_or_else(|e| {
            panic!("Relay needs the system FFmpeg and VA-API development packages ({name}): {e}")
        });
        build.includes(lib.include_paths);
    }
    build
        .file("src/session/gfx/vaapi.c")
        .warnings(true)
        .compile("relay_vaapi");
}
