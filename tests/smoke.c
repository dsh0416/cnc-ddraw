// Build this as x86 against the platform SDK. Load the DLL beside the test exe.
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <initguid.h>
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
typedef HRESULT (WINAPI *CreateExFn)(GUID *, void **, REFIID, IUnknown *);
typedef HRESULT (WINAPI *CreateClipperFn)(DWORD, IDirectDrawClipper **, IUnknown *);

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

static int exercise_interfaces(HMODULE dll, HWND window) {
    CreateExFn create_ex = (CreateExFn)GetProcAddress(dll, "DirectDrawCreateEx");
    CreateClipperFn create_clipper = (CreateClipperFn)GetProcAddress(dll, "DirectDrawCreateClipper");
    IDirectDraw7 *dd7 = NULL;
    IDirectDraw4 *dd4 = NULL;
    IDirectDraw2 *dd2 = NULL;
    IDirectDraw *dd1 = NULL;
    IDirectDrawSurface7 *surf7 = NULL, *from_dc = NULL;
    IDirectDrawSurface4 *surf4 = NULL;
    IDirectDrawSurface3 *surf3 = NULL;
    IDirectDrawSurface2 *surf2 = NULL;
    IDirectDrawGammaControl *gamma = NULL;
    IDirectDrawClipper *clipper = NULL, *standalone_clipper = NULL, *attached_clipper = NULL;
    DDSURFACEDESC2 desc2 = {0}, lock2 = {0};
    DDSCAPS2 caps2 = {DDSCAPS_OFFSCREENPLAIN};
    DDDEVICEIDENTIFIER2 devid2 = {0};
    DDGAMMARAMP ramp = {0}, ramp_out = {0};
    DDBLTFX fx = {0};
    DWORD total = 0, free_mem = 0, uniq1 = 0, uniq2 = 0, prio = 0, lod = 0;
    void *dd_iface = NULL;
    HDC dc = NULL;
    GUID tag = {0x11223344, 0x5566, 0x7788, {1, 2, 3, 4, 5, 6, 7, 8}};
    DWORD payload = 0xdeadbeef, payload_out = 0, payload_size = sizeof(payload_out);
    struct {
        RGNDATAHEADER hdr;
        RECT rc;
    } rgn;

    REQUIRE(create_ex && create_clipper);
    CHECK(create_clipper(0, &standalone_clipper, NULL));
    REQUIRE(IDirectDrawClipper_Release(standalone_clipper) == 0);

    CHECK(create_ex(NULL, (void **)&dd7, &IID_IDirectDraw7, NULL));
    CHECK(IDirectDraw7_QueryInterface(dd7, &IID_IDirectDraw4, (void **)&dd4));
    CHECK(IDirectDraw7_QueryInterface(dd7, &IID_IDirectDraw2, (void **)&dd2));
    CHECK(IDirectDraw7_QueryInterface(dd7, &IID_IDirectDraw, (void **)&dd1));
    CHECK(IDirectDraw7_TestCooperativeLevel(dd7));
    CHECK(IDirectDraw7_RestoreAllSurfaces(dd7));
    CHECK(IDirectDraw7_GetDeviceIdentifier(dd7, &devid2, 0));
    REQUIRE(strcmp(devid2.szDriver, "ddraw.dll") == 0);
    CHECK(IDirectDraw7_GetAvailableVidMem(dd7, &caps2, &total, &free_mem));
    REQUIRE(total > 0 && free_mem <= total);
    CHECK(IDirectDraw7_SetDisplayMode(dd7, 64, 64, 32, 60, 0));
    if (window) {
        CHECK(IDirectDraw7_SetCooperativeLevel(dd7, window, DDSCL_NORMAL));
    }

    desc2.dwSize = sizeof(desc2);
    desc2.dwFlags = DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT;
    desc2.ddsCaps.dwCaps = DDSCAPS_OFFSCREENPLAIN;
    desc2.dwWidth = 16;
    desc2.dwHeight = 16;
    CHECK(IDirectDraw7_CreateSurface(dd7, &desc2, &surf7, NULL));
    CHECK(IDirectDrawSurface7_QueryInterface(surf7, &IID_IDirectDrawSurface4, (void **)&surf4));
    CHECK(IDirectDrawSurface7_QueryInterface(surf7, &IID_IDirectDrawSurface3, (void **)&surf3));
    CHECK(IDirectDrawSurface7_QueryInterface(surf7, &IID_IDirectDrawSurface2, (void **)&surf2));
    CHECK(IDirectDrawSurface7_QueryInterface(surf7, &IID_IDirectDrawGammaControl, (void **)&gamma));

    // Clipper: clip list + clipped colorfill Blt.
    CHECK(IDirectDraw7_CreateClipper(dd7, 0, &clipper, NULL));
    rgn.hdr.dwSize = sizeof(RGNDATAHEADER);
    rgn.hdr.iType = RDH_RECTANGLES;
    rgn.hdr.nCount = 1;
    rgn.hdr.nRgnSize = sizeof(RECT);
    rgn.hdr.rcBound.left = 2;
    rgn.hdr.rcBound.top = 2;
    rgn.hdr.rcBound.right = 6;
    rgn.hdr.rcBound.bottom = 6;
    rgn.rc = rgn.hdr.rcBound;
    CHECK(IDirectDrawClipper_SetClipList(clipper, (RGNDATA *)&rgn, 0));
    CHECK(IDirectDrawSurface7_SetClipper(surf7, clipper));
    CHECK(IDirectDrawSurface7_GetClipper(surf7, &attached_clipper));
    REQUIRE(attached_clipper == clipper);
    IDirectDrawClipper_Release(attached_clipper);

    fx.dwSize = sizeof(fx);
    fx.dwFillColor = 0x00112233;
    CHECK(IDirectDrawSurface7_Blt(surf7, NULL, NULL, NULL, DDBLT_COLORFILL, &fx));
    lock2.dwSize = sizeof(lock2);
    CHECK(IDirectDrawSurface7_Lock(surf7, NULL, &lock2, 0, NULL));
    REQUIRE(((DWORD *)lock2.lpSurface)[0] == 0);
    REQUIRE(*(DWORD *)((BYTE *)lock2.lpSurface + 2 * lock2.lPitch + 2 * 4) == 0x00112233);
    CHECK(IDirectDrawSurface7_Unlock(surf7, NULL));

    // Gamma control.
    CHECK(IDirectDrawGammaControl_GetGammaRamp(gamma, 0, &ramp));
    REQUIRE(ramp.red[255] == 0xffff && ramp.red[0] == 0);
    ramp.red[10] = 0x4321;
    CHECK(IDirectDrawGammaControl_SetGammaRamp(gamma, 0, &ramp));
    CHECK(IDirectDrawGammaControl_GetGammaRamp(gamma, 0, &ramp_out));
    REQUIRE(ramp_out.red[10] == 0x4321);

    // GetSurfaceFromDC + GetDDInterface + PrivateData + Uniqueness + Priority + LOD.
    CHECK(IDirectDrawSurface7_GetDC(surf7, &dc));
    CHECK(IDirectDraw7_GetSurfaceFromDC(dd7, dc, &from_dc));
    REQUIRE(from_dc != NULL);
    IDirectDrawSurface7_Release(from_dc);
    CHECK(IDirectDrawSurface7_ReleaseDC(surf7, dc));

    CHECK(IDirectDrawSurface7_GetDDInterface(surf7, &dd_iface));
    REQUIRE(dd_iface != NULL);
    IUnknown_Release((IUnknown *)dd_iface);

    CHECK(IDirectDrawSurface7_PageLock(surf7, 0));
    CHECK(IDirectDrawSurface7_PageUnlock(surf7, 0));
    CHECK(IDirectDrawSurface7_GetUniquenessValue(surf7, &uniq1));
    CHECK(IDirectDrawSurface7_ChangeUniquenessValue(surf7));
    CHECK(IDirectDrawSurface7_GetUniquenessValue(surf7, &uniq2));
    REQUIRE(uniq2 != uniq1);
    CHECK(IDirectDrawSurface7_SetPriority(surf7, 42));
    CHECK(IDirectDrawSurface7_GetPriority(surf7, &prio));
    REQUIRE(prio == 42);
    CHECK(IDirectDrawSurface7_SetLOD(surf7, 3));
    CHECK(IDirectDrawSurface7_GetLOD(surf7, &lod));
    REQUIRE(lod == 3);
    CHECK(IDirectDrawSurface7_SetPrivateData(surf7, &tag, &payload, sizeof(payload), 0));
    CHECK(IDirectDrawSurface7_GetPrivateData(surf7, &tag, &payload_out, &payload_size));
    REQUIRE(payload_out == payload && payload_size == sizeof(payload));
    CHECK(IDirectDrawSurface7_FreePrivateData(surf7, &tag));
    REQUIRE(FAILED(IDirectDrawSurface7_GetPrivateData(surf7, &tag, &payload_out, &payload_size)));

    IDirectDrawClipper_Release(clipper);
    IDirectDrawGammaControl_Release(gamma);
    IDirectDrawSurface2_Release(surf2);
    IDirectDrawSurface3_Release(surf3);
    IDirectDrawSurface4_Release(surf4);
    IDirectDrawSurface7_Release(surf7);
    IDirectDraw_Release(dd1);
    IDirectDraw2_Release(dd2);
    IDirectDraw4_Release(dd4);
    REQUIRE(IDirectDraw7_Release(dd7) == 0);
    puts("passed DD2/4/7, Surface2/3/4/7, Clipper & GammaControl");
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
    REQUIRE(exercise_interfaces(dll, window) == 0);
    if (window) DestroyWindow(window);
    FreeLibrary(dll);
    puts("x86 DirectDraw smoke test passed");
    return 0;
}
