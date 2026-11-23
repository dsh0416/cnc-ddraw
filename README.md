# cnc-ddraw — Rust / wgpu experiment

This branch builds a 32-bit Windows `ddraw.dll` from Rust. A DirectDraw 1 client calls `DirectDrawCreate`, binds its HWND with `SetCooperativeLevel`, sets a logical display mode, and creates a primary surface. Updates to that surface produce frames through wgpu. The default Cargo target is `i686-pc-windows-msvc`.

## Build on Windows

Install Rust and the Visual Studio C++ x86 tools with the Windows SDK. From the **x86 Native Tools Command Prompt**, run

```bat
rustup target add i686-pc-windows-msvc
cargo build --locked --release
```

The DLL is written to `target\i686-pc-windows-msvc\release\ddraw.dll`. Place it beside a 32-bit DirectDraw 1 executable. wgpu selects the adapter; set `WGPU_BACKEND=dx12` or `WGPU_BACKEND=vulkan` to select one backend explicitly.

To cross-compile on macOS or Linux, put the LLVM-MinGW UCRT toolchain's `bin` directory on PATH and run

```sh
rustup target add i686-pc-windows-gnullvm
cargo build --locked --release --target i686-pc-windows-gnullvm
```

The output is `target/i686-pc-windows-gnullvm/release/ddraw.dll`. To load the local build in Wine, use the native DLL override `WINEDLLOVERRIDES=ddraw=n`.

## Implementation

- `src/draw.rs` implements the DLL exports, COM device, mode enumeration and window binding. Exclusive fullscreen requests use a borderless window on the current monitor. Exiting restores the window's saved placement and styles.
- `src/surface.rs` owns the CPU pixels of primary and offscreen surfaces. It supports rectangular locks, color fills, same-format copies, stretching, source color keys, one attached flip buffer, and GDI drawing through a DIB. Locks and DCs exclude concurrent writes and flips.
- `src/palette.rs` stores the 256-entry palette. Changing an attached palette refreshes the primary image even when the pixel indices stay the same.
- `src/renderer.rs` and `src/present.wgsl` share one presentation pipeline across Vulkan and DX12. Indexed, RGB555, RGB565 and XRGB8888 pixels are expanded on the CPU and uploaded as RGBA. The shader scales with nearest-neighbor sampling into a centered, aspect-preserving viewport.

The implemented interface is single-threaded DirectDraw 1. Release the display and its surfaces, or detach the HWND with `SetCooperativeLevel(NULL, DDSCL_NORMAL)`, before destroying the window. Release the primary surface before changing its logical mode. Later DirectDraw interfaces, clippers, overlays, vblank queries, and Direct3D game rendering are outside this experiment's implemented API. Unsupported methods return a failing HRESULT.

## Verify

The SDK smoke test is a separate C executable. It loads the DLL next to the executable by path, resolves the factory by name, and calls it using the SDK's x86 COM declarations.

```bat
cargo test --locked
cl /nologo tests\smoke.c user32.lib gdi32.lib /Fe:target\i686-pc-windows-msvc\release\smoke.exe
target\i686-pc-windows-msvc\release\smoke.exe
target\i686-pc-windows-msvc\release\smoke.exe --present
```

The default run checks pixel reads, odd-width pitches, palette updates, GDI writes, overlapping blits, busy surfaces and flipping at 8, 16 and 32 bits. `--present` also creates a real window, presents through wgpu, and checks fullscreen entry and restoration. CI runs the CPU/ABI suite and uploads the release DLL.
