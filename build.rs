// Compile stubs for SDL2 Metal symbols that rust-sdl2 0.36 references
// unconditionally in its WindowContext Drop impl. Required on Linux targets
// whose libSDL2 predates the 2.0.18 stub (e.g. Ubuntu 20.04 ships 2.0.10,
// which is what the cross-rs Docker images use for aarch64/armv7).
//
// See src/sdl_metal_stubs.c for the full explanation.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    println!("cargo:rerun-if-changed=src/sdl_metal_stubs.c");
    println!("cargo:rerun-if-changed=build.rs");

    if target.contains("linux") {
        cc::Build::new()
            .file("src/sdl_metal_stubs.c")
            .compile("picogallery_sdl_metal_stubs");
    }
}
