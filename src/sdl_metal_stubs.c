/*
 * Stub implementations for SDL2 Metal symbols on Linux.
 *
 * The rust-sdl2 0.36 crate's Drop impl for WindowContext unconditionally
 * references SDL_Metal_DestroyView, even on non-Apple targets. SDL2 < 2.0.18
 * (Ubuntu 20.04 ships 2.0.10) does not provide a stub for this symbol on
 * Linux, so the binary fails to link with:
 *
 *   undefined reference to `SDL_Metal_DestroyView'
 *
 * These stubs are no-ops. They are only invoked by rust-sdl2 when
 * `WindowContext.metal_view` is non-null, which never happens on Linux
 * because the Metal view APIs (SDL_Metal_CreateView) are Apple-only.
 *
 * On newer SDL2 versions where libSDL2 also defines these symbols, the
 * linker pulls in our static archive first to resolve the undefined
 * reference from picogallery's own object files — so these stubs are
 * used for the executable's copy of the symbol, with no conflict.
 */

void SDL_Metal_DestroyView(void *view) {
    (void)view;
}
