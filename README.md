# cnc-ddraw — Rust / wgpu experiment

This branch builds a 32-bit Windows `ddraw.dll` from Rust. A DirectDraw 1–7 client calls `DirectDrawCreate`, `DirectDrawCreateEx`, or COM `CoCreateInstance`, binds its HWND with `SetCooperativeLevel`, sets a logical display mode, and creates primary or offscreen surfaces. Updates to the primary surface produce frames through wgpu. The default Cargo target is `i686-pc-windows-msvc`.

## Supported Games

- Command & Conquer: Red Alert 2
- Prince of Qin (秦殇)
- Road Rash

## Build on Windows

Install Rust and the Visual Studio C++ x86 tools with the Windows SDK. From the **x86 Native Tools Command Prompt**, run

```bat
rustup target add i686-pc-windows-msvc
cargo build --locked --release
```

The DLL is written to `target\i686-pc-windows-msvc\release\ddraw.dll`. Place it beside a 32-bit DirectDraw executable. wgpu selects the adapter; set `WGPU_BACKEND=dx12` or `WGPU_BACKEND=vulkan` to select one backend explicitly.

To cross-compile on macOS or Linux, put the LLVM-MinGW UCRT toolchain's `bin` directory on PATH and run

```sh
rustup target add i686-pc-windows-gnullvm
cargo build --locked --release --target i686-pc-windows-gnullvm
```

The output is `target/i686-pc-windows-gnullvm/release/ddraw.dll`. To load the local build in Wine, use the native DLL override `WINEDLLOVERRIDES=ddraw=n`.

## Configuration (`ddraw.ini`)

Place an optional `ddraw.ini` next to `ddraw.dll` (or the game executable) using standard `[ddraw]` keys (with optional `[<exe_name>]` per-game overrides):

```ini
[ddraw]
windowed=false           ; start in windowed mode instead of borderless fullscreen
border=true              ; show window titlebar/frame in windowed mode
resizable=true           ; allow dragging window edges to resize
width=0                  ; windowed client width (0 = follow game logical resolution)
height=0                 ; windowed client height (0 = follow game logical resolution)
posX=-32000              ; windowed X position (-32000 = center on monitor)
posY=-32000              ; windowed Y position (-32000 = center on monitor)
maintas=true             ; preserve aspect ratio (false = stretch to fill window)
vsync=false              ; true = Fifo vsync, false = low-latency Mailbox/Immediate
maxfps=0                 ; 0 = coalesced ~250 Hz cap, >0 = explicit FPS cap
renderer=auto            ; auto, dx12, or vulkan (overridden by WGPU_BACKEND env var)
shader=                  ; optional RetroArch .glslp preset or standalone .glsl shader (or CNC_DDRAW_SHADER env var)
keytogglefullscreen=0x0D ; Alt+key toggle between borderless fullscreen and windowed (0 = off)
singlecpu=true           ; pin game-directory threads to CPU0 to prevent multi-core engine freezes
noactivateapp=false      ; shield focus deactivation messages (auto-enabled for RA2/TS presets)
log=false                ; write diagnostic events to cnc-ddraw.log (or set CNC_DDRAW_LOG=path)
```

Press **Alt+Enter** at any time to toggle between borderless fullscreen and windowed mode.

## Implementation

The Cargo workspace is organized into three crates:

- **Root crate `cnc-ddraw` (`src/`, builds `ddraw.dll`)**: Pure DirectDraw 1–7 COM/surface/presentation implementation and DLL exports:
  - `src/draw/` implements `IDirectDraw`, `IDirectDraw2`, `IDirectDraw4`, and `IDirectDraw7`, along with `DirectDrawCreate`, `DirectDrawCreateEx`, `DirectDrawCreateClipper`, device/mode/surface enumeration, vblank/scanline timing, and coalesced frame flushing.
  - `src/surface/` implements `IDirectDrawSurface` 1–7 and `IDirectDrawGammaControl` with CPU-backed pixels, rectangular locks, fills, ROP/color-keyed blits, stretching, mirroring, clippers, gamma ramps, and GDI DIB drawing.
  - `src/simd_blit.rs` uses `fearless_simd` for runtime-dispatched (SSE2 / SSE4.2 / AVX2 / AVX-512) branchless CPU color-keyed blits and fills.
  - `src/clipper.rs`, `src/palette.rs`, `src/ffi/`, and `src/exports.rs` (`exports.def`, `build.rs`) handle clippers, 256-entry palettes, checked COM descriptors, ordinals `@1`..`@21`, `DllMain`, and `IClassFactory`.
  - `src/renderer.rs` and `src/present.wgsl` share one wgpu presentation pipeline across Vulkan and DX12, uploading raw surface bytes (`R8Uint`/`R16Uint`/`R32Uint`) and unpacking via a 256-entry palette/gamma LUT.
- **`crates/win32-hooks`**: Win32 host windowing, presentation overlay (`OverlayWindow`), logical↔physical input mapping, WndProc subclass / `WH_GETMESSAGE` hooks, DPI dialog scaling, `retour` hotpatch hooks (`win32/original.rs`), `ddraw.ini` config parsing (`config.rs`), and CPU0 affinity / logging (`compat.rs`). Communicates with the display owner through `window::HostCallbacks` with zero reverse dependency on DirectDraw.
- **`crates/shader-glslp`** (`#![forbid(unsafe_code)]`, platform-independent): Pure-Rust RetroArch `.glslp` preset and standalone `.glsl` (including `.glsl.pass1`) filter chain on top of `wgpu` + `naga`. When `shader=` is configured, Pass 0 unpacks the raw frame into an `Rgba8Unorm` `Original` target, and the shader chain translates legacy RetroArch GLSL (110–130 + `COMPAT_*` macros + `#pragma parameter` + PNG LUTs + multi-pass FBOs/aliases/frame history) into Vulkan GLSL 450 compiled through `naga` across both DX12 and Vulkan. Invalid shaders log diagnostics and fall back cleanly to the default presenter.

`src/descriptor.rs` checks DirectDraw structure sizes at the COM boundary and reads the fields selected by the caller's flags. Output descriptors can arrive with only `dwSize` initialized. Set `DDPF_RGB` when supplying true-color depths and masks. `EnumDisplayModes` treats a pixel format with no flags as a wildcard. Surface creation and resizing copy the visible pixels from each input row. `src/surface/dc.rs` owns the GDI bitmap and accounts for its scanline stride separately from the game's pitch.

The CPU blitter, SIMD kernels, configuration parser and dialog template parser use `#![forbid(unsafe_code)]`. The crate also enforces `unsafe_op_in_unsafe_fn`, so native entry points need an explicit block for each group of unsafe operations. For DPI scaling, indirect standard dialog templates with nonzero control creation data use the original Win32 template. Resource-backed templates have a known length and can be copied in full.

The implemented DirectDraw COM objects are single-threaded. Release the display and its surfaces, or detach the HWND with `SetCooperativeLevel(NULL, DDSCL_NORMAL)`, before destroying the window. Direct3D 3D game rendering remains outside this experiment's scope.

## Verify

The SDK smoke test is a separate C executable. It loads the DLL next to the executable by path, resolves exports by name and ordinal, and calls the interfaces using the SDK's x86 COM declarations.

```bat
cargo test --locked
cl /nologo tests\smoke.c user32.lib gdi32.lib /Fe:target\i686-pc-windows-msvc\release\smoke.exe
target\i686-pc-windows-msvc\release\smoke.exe
target\i686-pc-windows-msvc\release\smoke.exe --present
```

The default run checks 8/16/32-bit surfaces, odd-width pitches, palettes, GDI writes, overlapping blits, busy surfaces, flipping, DD2/4/7 and Surface2/3/4/7 interfaces, clippers, gamma control, multi-backbuffer chains, Blt FX/ROP/batching, overlays, vblank/scanline queries, export ordinals, thread lock, and COM class factories. `--present` also creates a real window, presents through wgpu, and checks fullscreen entry and restoration.
