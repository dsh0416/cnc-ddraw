// Build this as x86 against the platform SDK. Load the DLL beside the test exe.
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <ddraw.h>
#include <stdio.h>
#include <string.h>
#include <wchar.h>

#define CHECK(expr) do { HRESULT hr_ = (expr); if (FAILED(hr_)) { \
    fprintf(stderr, "%s:%d: %s -> 0x%08lx\n", __FILE__, __LINE__, #expr, (unsigned long)hr_); return 1; \
} } while (0)
#define REQUIRE(expr) do { if (!(expr)) { \
    fprintf(stderr, "%s:%d: %s failed\n", __FILE__, __LINE__, #expr); return 1; \
} } while (0)

typedef HRESULT (WINAPI *CreateFn)(GUID *, IDirectDraw **, IUnknown *);

static int exercise(IDirectDraw *draw, unsigned depth) {
    DDSURFACEDESC desc = {0}, lock = {0};
    IDirectDrawSurface *front = NULL, *back = NULL;
    IDirectDrawPalette *palette = NULL;
    DDSCAPS caps = {DDSCAPS_BACKBUFFER};
    DDBLTFX fx = {0};
    PALETTEENTRY colors[256] = {{0}};
    const unsigned width = 321, height = 201, bytes = depth / 8;
    const DWORD color = depth == 8 ? 77 : depth == 16 ? 0xf800 : 0x00204080;
    unsigned i;

    CHECK(IDirectDraw_SetDisplayMode(draw, width, height, depth));
    desc.dwSize = sizeof(desc);
    desc.dwFlags = DDSD_CAPS | DDSD_BACKBUFFERCOUNT;
    desc.ddsCaps.dwCaps = DDSCAPS_PRIMARYSURFACE | DDSCAPS_FLIP | DDSCAPS_COMPLEX;
    desc.dwBackBufferCount = 1;
    CHECK(IDirectDraw_CreateSurface(draw, &desc, &front, NULL));
    CHECK(IDirectDrawSurface_GetAttachedSurface(front, &caps, &back));
    if (depth == 8) {
        colors[color].peRed = 200;
        CHECK(IDirectDraw_CreatePalette(draw, DDPCAPS_8BIT | DDPCAPS_ALLOW256, colors, &palette, NULL));
        CHECK(IDirectDrawSurface_SetPalette(front, palette));
    }

    fx.dwSize = sizeof(fx);
    fx.dwFillColor = color;
    CHECK(IDirectDrawSurface_Blt(back, NULL, NULL, NULL, DDBLT_COLORFILL, &fx));
    lock.dwSize = sizeof(lock);
    if (depth == 16) {
        HDC dc = NULL;
        CHECK(IDirectDrawSurface_GetDC(back, &dc));
        REQUIRE(GetPixel(dc, 0, 0) == RGB(255, 0, 0));
        REQUIRE(SetPixelV(dc, 1, 1, RGB(0, 0, 255)));
        REQUIRE(FAILED(IDirectDrawSurface_Lock(back, NULL, &lock, 0, NULL)));
        REQUIRE(FAILED(IDirectDrawSurface_Flip(front, NULL, 0)));
        CHECK(IDirectDrawSurface_ReleaseDC(back, dc));
    }
    CHECK(IDirectDrawSurface_Lock(back, NULL, &lock, 0, NULL));
    REQUIRE(lock.lPitch >= (LONG)(width * bytes));
    REQUIRE(lock.ddpfPixelFormat.dwRGBBitCount == depth);
    if (depth == 16) {
        WORD value;
        memcpy(&value, (BYTE *)lock.lpSurface + lock.lPitch + 2, sizeof(value));
        REQUIRE(value == 0x001f);
    }
    REQUIRE(memcmp((BYTE *)lock.lpSurface + (height - 1) * lock.lPitch + (width - 1) * bytes,
                   &color, bytes) == 0);
    REQUIRE(FAILED(IDirectDrawSurface_Flip(front, NULL, 0)));
    for (i = 0; i < width; ++i) {
        memset((BYTE *)lock.lpSurface + i * bytes, 0xff, bytes);
    }
    CHECK(IDirectDrawSurface_Unlock(back, lock.lpSurface));
    CHECK(IDirectDrawSurface_Flip(front, NULL, DDFLIP_WAIT));
    CHECK(IDirectDrawSurface_Lock(front, NULL, &lock, 0, NULL));
    REQUIRE(((BYTE *)lock.lpSurface)[0] == 0xff);
    REQUIRE(memcmp((BYTE *)lock.lpSurface + lock.lPitch, &color, bytes) == 0);
    CHECK(IDirectDrawSurface_Unlock(front, lock.lpSurface));
    if (palette) {
        PALETTEENTRY readback;
        colors[color].peBlue = 128;
        CHECK(IDirectDrawPalette_SetEntries(palette, 0, color, 1, colors + color));
        CHECK(IDirectDrawPalette_GetEntries(palette, 0, color, 1, &readback));
        REQUIRE(readback.peBlue == 128);
        IDirectDrawPalette_Release(palette);
    }
    // Move the first row down using an overlapping self-blit.
    {
        RECT source = {0, 0, 2, 2};
        CHECK(IDirectDrawSurface_BltFast(front, 0, 1, front, &source, 0));
        CHECK(IDirectDrawSurface_Lock(front, NULL, &lock, 0, NULL));
        REQUIRE(((BYTE *)lock.lpSurface)[lock.lPitch] == 0xff);
        REQUIRE(memcmp((BYTE *)lock.lpSurface + lock.lPitch * 2, &color, bytes) == 0);
        CHECK(IDirectDrawSurface_Unlock(front, lock.lpSurface));
    }
    IDirectDrawSurface_Release(back);
    IDirectDrawSurface_Release(front);
    printf("passed %u-bit surfaces\n", depth);
    return 0;
}

int main(int argc, char **argv) {
    wchar_t path[MAX_PATH];
    const DWORD len = GetModuleFileNameW(NULL, path, MAX_PATH);
    wchar_t *base;
    HMODULE dll;
    CreateFn create;
    IDirectDraw *draw = NULL;
    HWND window = NULL;
    REQUIRE(sizeof(void *) == 4 && len && len < MAX_PATH);
    base = wcsrchr(path, L'\\');
    REQUIRE(base && (size_t)(base - path) + 11 < MAX_PATH);
    wcscpy(base + 1, L"ddraw.dll");
    dll = LoadLibraryW(path);
    if (!dll) fprintf(stderr, "LoadLibraryW failed with Win32 error %lu\n", GetLastError());
    REQUIRE(dll);
    create = (CreateFn)GetProcAddress(dll, "DirectDrawCreate");
    REQUIRE(create);
    CHECK(create(NULL, &draw, NULL));
    if (argc == 2 && strcmp(argv[1], "--present") == 0) {
        window = CreateWindowExW(0, L"STATIC", L"cnc-ddraw wgpu test", WS_OVERLAPPEDWINDOW,
                                 CW_USEDEFAULT, CW_USEDEFAULT, 640, 480, NULL, NULL,
                                 GetModuleHandleW(NULL), NULL);
        REQUIRE(window);
        ShowWindow(window, SW_SHOW);
        UpdateWindow(window);
        CHECK(IDirectDraw_SetCooperativeLevel(draw, window, DDSCL_NORMAL));
    } else {
        REQUIRE(argc == 1);
    }
    REQUIRE(exercise(draw, 8) == 0);
    if (window) {
        LONG old_style = GetWindowLongW(window, GWL_STYLE);
        RECT old_rect, fullscreen, restored;
        MONITORINFO monitor = {0};
        monitor.cbSize = sizeof(monitor);
        REQUIRE(GetWindowRect(window, &old_rect));
        REQUIRE(GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &monitor));
        CHECK(IDirectDraw_SetCooperativeLevel(draw, window, DDSCL_FULLSCREEN | DDSCL_EXCLUSIVE));
        REQUIRE(GetWindowRect(window, &fullscreen));
        REQUIRE(EqualRect(&monitor.rcMonitor, &fullscreen));
        REQUIRE(exercise(draw, 16) == 0);
        CHECK(IDirectDraw_SetCooperativeLevel(draw, window, DDSCL_NORMAL));
        REQUIRE(GetWindowLongW(window, GWL_STYLE) == old_style);
        REQUIRE(GetWindowRect(window, &restored) && EqualRect(&old_rect, &restored));
    } else {
        REQUIRE(exercise(draw, 16) == 0);
    }
    REQUIRE(exercise(draw, 32) == 0);
    CHECK(IDirectDraw_RestoreDisplayMode(draw));
    REQUIRE(IDirectDraw_Release(draw) == 0);
    if (window) DestroyWindow(window);
    FreeLibrary(dll);
    puts("x86 DirectDraw smoke test passed");
    return 0;
}
