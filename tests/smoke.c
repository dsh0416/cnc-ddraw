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
typedef DWORD (WINAPI *ThreadLockFn)(void);
typedef DWORD (WINAPI *SysmemFn)(DWORD);
typedef HRESULT (WINAPI *ParseCmdFn)(void *, void **);
typedef DWORD (WINAPI *InternalLockFn)(DWORD, DWORD);
typedef DWORD (WINAPI *InternalUnlockFn)(DWORD);
typedef BOOL (__cdecl *IsWindowedFn)(void);
typedef void (__cdecl *EnableZoomFn)(void);
typedef FARPROC (WINAPI *DdGetProcAddressFn)(HMODULE, LPCSTR);
typedef HRESULT (WINAPI *DllGetClassObjectFn)(REFCLSID, REFIID, void **);
typedef HRESULT (WINAPI *DllCanUnloadNowFn)(void);
typedef BOOL (WINAPI *GetClientRectFn)(HWND, LPRECT);

static HRESULT WINAPI count_modes_cb(DDSURFACEDESC2 *desc, void *ctx) {
    unsigned *count = (unsigned *)ctx;
    if (desc->dwFlags & DDSD_REFRESHRATE) {
        (*count)++;
    }
    return DDENUMRET_OK;
}

static HRESULT WINAPI count_surfaces_cb(IDirectDrawSurface7 *surf, DDSURFACEDESC2 *desc, void *ctx) {
    unsigned *count = (unsigned *)ctx;
    (void)desc;
    (*count)++;
    IDirectDrawSurface7_Release(surf);
    return DDENUMRET_OK;
}

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
        const DWORD palette_caps = DDPCAPS_8BIT | DDPCAPS_ALLOW256;
        IDirectDrawPalette *legacy_palette = NULL;
        PALETTEENTRY readback[256] = {{0}};
        DWORD reported_caps = 0;
        colors[color].peRed = 200;
        // Road Rash passes the original binary value of DDPCAPS_INITIALIZE.
        CHECK(IDirectDraw_CreatePalette(draw, palette_caps | 0x08, colors, &legacy_palette, NULL));
        CHECK(IDirectDrawPalette_GetCaps(legacy_palette, &reported_caps));
        REQUIRE(reported_caps == palette_caps);
        CHECK(IDirectDrawPalette_GetEntries(legacy_palette, 0, 0, 256, readback));
        REQUIRE(memcmp(readback, colors, sizeof(colors)) == 0);
        IDirectDrawPalette_Release(legacy_palette);
        CHECK(IDirectDraw_CreatePalette(draw, palette_caps, colors, &palette, NULL));
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
        CHECK(IDirectDraw7_SetCooperativeLevel(
            dd7, window, DDSCL_NORMAL | DDSCL_NOWINDOWCHANGES | DDSCL_FPUSETUP));
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

static int exercise_compat(HMODULE dll) {
    CreateExFn create_ex = (CreateExFn)GetProcAddress(dll, "DirectDrawCreateEx");
    IDirectDraw7 *dd7 = NULL;
    IDirectDrawSurface7 *primary = NULL, *back1 = NULL, *back2 = NULL, *gdi = NULL;
    IDirectDrawSurface7 *off = NULL, *dup = NULL, *extra = NULL, *overlay = NULL;
    DDSURFACEDESC2 desc = {0}, lock = {0};
    DDSCAPS2 back_caps = {DDSCAPS_BACKBUFFER};
    DDBLTFX fx = {0};
    DDBLTBATCH batch = {0};
    BOOL in_vb = FALSE;
    DWORD scanline = 0;
    LONG ox = 0, oy = 0;
    unsigned mode_count = 0, surf_count = 0;
    HRESULT hr;

    CHECK(create_ex(NULL, (void **)&dd7, &IID_IDirectDraw7, NULL));
    CHECK(IDirectDraw7_SetDisplayMode(dd7, 32, 32, 32, 60, 0));

    // EnumDisplayModes with refresh rates flag.
    CHECK(IDirectDraw7_EnumDisplayModes(
        dd7, DDEDM_REFRESHRATES | DDEDM_STANDARDVGAMODES, NULL, &mode_count, count_modes_cb));
    REQUIRE(mode_count > 0);

    // VBlank & ScanLine queries.
    CHECK(IDirectDraw7_GetVerticalBlankStatus(dd7, &in_vb));
    hr = IDirectDraw7_GetScanLine(dd7, &scanline);
    REQUIRE(SUCCEEDED(hr) || hr == DDERR_VERTICALBLANKINPROGRESS);
    CHECK(IDirectDraw7_WaitForVerticalBlank(dd7, DDWAITVB_BLOCKBEGIN, NULL));

    // Triple buffering (2 backbuffers) + video memory caps.
    desc.dwSize = sizeof(desc);
    desc.dwFlags = DDSD_CAPS | DDSD_BACKBUFFERCOUNT;
    desc.ddsCaps.dwCaps =
        DDSCAPS_PRIMARYSURFACE | DDSCAPS_FLIP | DDSCAPS_COMPLEX | DDSCAPS_VIDEOMEMORY;
    desc.dwBackBufferCount = 2;
    CHECK(IDirectDraw7_CreateSurface(dd7, &desc, &primary, NULL));
    CHECK(IDirectDrawSurface7_GetAttachedSurface(primary, &back_caps, &back1));
    CHECK(IDirectDrawSurface7_GetAttachedSurface(back1, &back_caps, &back2));

    // Fill front=1, back1=2, back2=3 and rotate via Flip.
    fx.dwSize = sizeof(fx);
    fx.dwFillColor = 0x00010101;
    CHECK(IDirectDrawSurface7_Blt(primary, NULL, NULL, NULL, DDBLT_COLORFILL, &fx));
    fx.dwFillColor = 0x00020202;
    CHECK(IDirectDrawSurface7_Blt(back1, NULL, NULL, NULL, DDBLT_COLORFILL, &fx));
    fx.dwFillColor = 0x00030303;
    CHECK(IDirectDrawSurface7_Blt(back2, NULL, NULL, NULL, DDBLT_COLORFILL, &fx));
    CHECK(IDirectDrawSurface7_Flip(primary, NULL, DDFLIP_WAIT));
    lock.dwSize = sizeof(lock);
    CHECK(IDirectDrawSurface7_Lock(primary, NULL, &lock, 0, NULL));
    REQUIRE(((DWORD *)lock.lpSurface)[0] == 0x00020202);
    CHECK(IDirectDrawSurface7_Unlock(primary, NULL));
    CHECK(IDirectDrawSurface7_Flip(primary, NULL, DDFLIP_WAIT));
    CHECK(IDirectDrawSurface7_Lock(primary, NULL, &lock, 0, NULL));
    REQUIRE(((DWORD *)lock.lpSurface)[0] == 0x00030303);
    CHECK(IDirectDrawSurface7_Unlock(primary, NULL));

    // GetGDISurface + FlipToGDISurface + DuplicateSurface rejected for primary.
    CHECK(IDirectDraw7_GetGDISurface(dd7, &gdi));
    REQUIRE(gdi != NULL);
    IDirectDrawSurface7_Release(gdi);
    CHECK(IDirectDraw7_FlipToGDISurface(dd7));
    REQUIRE(IDirectDraw7_DuplicateSurface(dd7, primary, &dup) == DDERR_CANTDUPLICATE);

    // Offscreen surface + DuplicateSurface + Add/DeleteAttachedSurface.
    memset(&desc, 0, sizeof(desc));
    desc.dwSize = sizeof(desc);
    desc.dwFlags = DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT;
    desc.ddsCaps.dwCaps = DDSCAPS_OFFSCREENPLAIN | DDSCAPS_VIDEOMEMORY;
    desc.dwWidth = 8;
    desc.dwHeight = 8;
    CHECK(IDirectDraw7_CreateSurface(dd7, &desc, &off, NULL));
    fx.dwFillColor = 0x00aabbcc;
    CHECK(IDirectDrawSurface7_Blt(off, NULL, NULL, NULL, DDBLT_COLORFILL | DDBLT_ASYNC, &fx));
    CHECK(IDirectDraw7_DuplicateSurface(dd7, off, &dup));
    CHECK(IDirectDrawSurface7_Lock(dup, NULL, &lock, 0, NULL));
    REQUIRE(((DWORD *)lock.lpSurface)[0] == 0x00aabbcc);
    CHECK(IDirectDrawSurface7_Unlock(dup, NULL));

    CHECK(IDirectDraw7_CreateSurface(dd7, &desc, &extra, NULL));
    CHECK(IDirectDrawSurface7_AddAttachedSurface(off, extra));
    CHECK(IDirectDrawSurface7_DeleteAttachedSurface(off, 0, extra));

    // Blt with DDBLT_ROP (BLACKNESS/WHITENESS), KEYSRCOVERRIDE, MIRRORLEFTRIGHT, BltBatch.
    memset(&fx, 0, sizeof(fx));
    fx.dwSize = sizeof(fx);
    fx.dwROP = WHITENESS;
    CHECK(IDirectDrawSurface7_Blt(extra, NULL, NULL, NULL, DDBLT_ROP, &fx));
    CHECK(IDirectDrawSurface7_Lock(extra, NULL, &lock, 0, NULL));
    REQUIRE(((DWORD *)lock.lpSurface)[0] == 0xffffffff);
    CHECK(IDirectDrawSurface7_Unlock(extra, NULL));

    memset(&fx, 0, sizeof(fx));
    fx.dwSize = sizeof(fx);
    fx.dwDDFX = DDBLTFX_MIRRORLEFTRIGHT;
    fx.ddckSrcColorkey.dwColorSpaceLowValue = 0x00aabbcc;
    fx.ddckSrcColorkey.dwColorSpaceHighValue = 0x00aabbcc;
    CHECK(IDirectDrawSurface7_Blt(
        extra, NULL, off, NULL, DDBLT_DDFX | DDBLT_KEYSRCOVERRIDE, &fx));
    CHECK(IDirectDrawSurface7_Lock(extra, NULL, &lock, 0, NULL));
    // All source pixels matched source key -> destination stayed white.
    REQUIRE(((DWORD *)lock.lpSurface)[0] == 0xffffffff);
    CHECK(IDirectDrawSurface7_Unlock(extra, NULL));

    memset(&fx, 0, sizeof(fx));
    fx.dwSize = sizeof(fx);
    fx.dwROP = BLACKNESS;
    batch.dwFlags = DDBLT_ROP;
    batch.lpDDBltFx = &fx;
    CHECK(IDirectDrawSurface7_BltBatch(extra, &batch, 1, 0));
    CHECK(IDirectDrawSurface7_Lock(extra, NULL, &lock, 0, NULL));
    REQUIRE(((DWORD *)lock.lpSurface)[0] == 0);
    CHECK(IDirectDrawSurface7_Unlock(extra, NULL));

    // EnumSurfaces.
    CHECK(IDirectDraw7_EnumSurfaces(
        dd7, DDENUMSURFACES_ALL | DDENUMSURFACES_DOESEXIST, NULL, &surf_count, count_surfaces_cb));
    REQUIRE(surf_count >= 6);

    // Overlay surface vs non-overlay surface.
    REQUIRE(IDirectDrawSurface7_GetOverlayPosition(off, &ox, &oy) == DDERR_NOTAOVERLAYSURFACE);
    memset(&desc, 0, sizeof(desc));
    desc.dwSize = sizeof(desc);
    desc.dwFlags = DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT;
    desc.ddsCaps.dwCaps = DDSCAPS_OVERLAY | DDSCAPS_VIDEOMEMORY;
    desc.dwWidth = 4;
    desc.dwHeight = 4;
    CHECK(IDirectDraw7_CreateSurface(dd7, &desc, &overlay, NULL));
    fx.dwSize = sizeof(fx);
    fx.dwFillColor = 0x00445566;
    CHECK(IDirectDrawSurface7_Blt(overlay, NULL, NULL, NULL, DDBLT_COLORFILL, &fx));
    CHECK(IDirectDrawSurface7_SetOverlayPosition(overlay, 3, 5));
    CHECK(IDirectDrawSurface7_GetOverlayPosition(overlay, &ox, &oy));
    REQUIRE(ox == 3 && oy == 5);
    {
        RECT dst_rect = {4, 4, 8, 8};
        CHECK(IDirectDrawSurface7_UpdateOverlay(
            overlay, NULL, primary, &dst_rect, DDOVER_SHOW, NULL));
        CHECK(IDirectDrawSurface7_Lock(primary, NULL, &lock, 0, NULL));
        REQUIRE(*(DWORD *)((BYTE *)lock.lpSurface + 4 * lock.lPitch + 4 * 4) == 0x00445566);
        CHECK(IDirectDrawSurface7_Unlock(primary, NULL));
    }

    IDirectDrawSurface7_Release(overlay);
    IDirectDrawSurface7_Release(extra);
    IDirectDrawSurface7_Release(dup);
    IDirectDrawSurface7_Release(off);
    IDirectDrawSurface7_Release(back2);
    IDirectDrawSurface7_Release(back1);
    IDirectDrawSurface7_Release(primary);
    REQUIRE(IDirectDraw7_Release(dd7) == 0);
    puts("passed compatibility methods, multi-backbuffer, Blt/FX/ROP, EnumSurfaces & Overlays");
    return 0;
}

static int exercise_exports(HMODULE dll) {
    ThreadLockFn acquire = (ThreadLockFn)GetProcAddress(dll, MAKEINTRESOURCEA(1));
    SysmemFn sysmem = (SysmemFn)GetProcAddress(dll, MAKEINTRESOURCEA(2));
    ParseCmdFn parse_cmd = (ParseCmdFn)GetProcAddress(dll, MAKEINTRESOURCEA(3));
    InternalLockFn ilock = (InternalLockFn)GetProcAddress(dll, MAKEINTRESOURCEA(5));
    InternalUnlockFn iunlock = (InternalUnlockFn)GetProcAddress(dll, MAKEINTRESOURCEA(6));
    CreateFn create_ord = (CreateFn)GetProcAddress(dll, MAKEINTRESOURCEA(8));
    CreateClipperFn clipper_ord = (CreateClipperFn)GetProcAddress(dll, MAKEINTRESOURCEA(9));
    CreateExFn create_ex_ord = (CreateExFn)GetProcAddress(dll, MAKEINTRESOURCEA(10));
    ThreadLockFn release = (ThreadLockFn)GetProcAddress(dll, MAKEINTRESOURCEA(21));
    IsWindowedFn is_windowed = (IsWindowedFn)GetProcAddress(dll, "DDIsWindowed");
    EnableZoomFn enable_zoom = (EnableZoomFn)GetProcAddress(dll, "DDEnableZoom");
    DdGetProcAddressFn dd_get_proc = (DdGetProcAddressFn)GetProcAddress(dll, "DDGetProcAddress");
    DllGetClassObjectFn get_class = (DllGetClassObjectFn)GetProcAddress(dll, "DllGetClassObject");
    DllCanUnloadNowFn can_unload = (DllCanUnloadNowFn)GetProcAddress(dll, "DllCanUnloadNow");
    IClassFactory *factory = NULL;
    IDirectDraw7 *dd7 = NULL;
    IDirectDrawClipper *clipper = NULL;
    void *ret_cmd = (void *)0x1;

    REQUIRE(acquire && sysmem && parse_cmd && ilock && iunlock);
    REQUIRE(create_ord && clipper_ord && create_ex_ord && release);
    REQUIRE(is_windowed && enable_zoom && dd_get_proc && get_class && can_unload);
    REQUIRE(GetProcAddress(dll, MAKEINTRESOURCEA(11)) == GetProcAddress(dll, "DirectDrawEnumerateA"));
    REQUIRE(GetProcAddress(dll, MAKEINTRESOURCEA(12)) == GetProcAddress(dll, "DirectDrawEnumerateExA"));
    REQUIRE(GetProcAddress(dll, MAKEINTRESOURCEA(13)) == GetProcAddress(dll, "DirectDrawEnumerateExW"));
    REQUIRE(GetProcAddress(dll, MAKEINTRESOURCEA(14)) == GetProcAddress(dll, "DirectDrawEnumerateW"));
    REQUIRE(dd_get_proc(dll, "DirectDrawCreate") == (FARPROC)create_ord);

    REQUIRE(acquire() == 0);
    REQUIRE(acquire() == 0);
    REQUIRE(release() == 0);
    REQUIRE(release() == 0);
    REQUIRE(sysmem(0) == 0);
    REQUIRE(ilock(0, 0) == 0);
    REQUIRE(iunlock(0) == 0);
    REQUIRE(FAILED(parse_cmd(NULL, &ret_cmd)) && ret_cmd == NULL);
    REQUIRE(is_windowed() == TRUE);
    enable_zoom();
    REQUIRE(can_unload() == S_FALSE);

    // COM class factory for CLSID_DirectDraw7 + Initialize.
    CHECK(get_class(&CLSID_DirectDraw7, &IID_IClassFactory, (void **)&factory));
    CHECK(IClassFactory_CreateInstance(factory, NULL, &IID_IDirectDraw7, (void **)&dd7));
    CHECK(IDirectDraw7_Initialize(dd7, NULL));
    REQUIRE(IDirectDraw7_Initialize(dd7, NULL) == DDERR_ALREADYINITIALIZED);
    IDirectDraw7_Release(dd7);
    IClassFactory_Release(factory);

    // COM class factory for CLSID_DirectDrawClipper + Initialize.
    CHECK(get_class(&CLSID_DirectDrawClipper, &IID_IClassFactory, (void **)&factory));
    CHECK(IClassFactory_CreateInstance(factory, NULL, &IID_IDirectDrawClipper, (void **)&clipper));
    CHECK(IDirectDrawClipper_Initialize(clipper, NULL, 0));
    REQUIRE(IDirectDrawClipper_Initialize(clipper, NULL, 0) == DDERR_ALREADYINITIALIZED);
    IDirectDrawClipper_Release(clipper);
    IClassFactory_Release(factory);

    puts("passed DLL export ordinals, thread lock, helpers & COM class factory");
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
    GetClientRectFn cached_client_rect = NULL;
    RECT physical_client, client;
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
        cached_client_rect = (GetClientRectFn)GetProcAddress(GetModuleHandleW(L"user32.dll"), "GetClientRect");
        REQUIRE(cached_client_rect && cached_client_rect(window, &physical_client));
        CHECK(IDirectDraw_SetCooperativeLevel(draw, window, DDSCL_NORMAL));
    } else {
        REQUIRE(argc == 1);
    }
    REQUIRE(exercise(draw, 8) == 0);
    if (window) {
        LONG old_style = GetWindowLongW(window, GWL_STYLE);
        RECT old_rect, fullscreen, restored;
        RECT logical_client = {0, 0, 321, 201};
        MONITORINFO monitor = {0};
        monitor.cbSize = sizeof(monitor);
        REQUIRE(GetWindowRect(window, &old_rect));
        REQUIRE(GetMonitorInfoW(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST), &monitor));
        CHECK(IDirectDraw_SetCooperativeLevel(draw, window, DDSCL_FULLSCREEN | DDSCL_EXCLUSIVE));
        REQUIRE(GetWindowRect(window, &fullscreen));
        REQUIRE(EqualRect(&monitor.rcMonitor, &fullscreen));
        REQUIRE(exercise(draw, 16) == 0);
        REQUIRE(cached_client_rect(window, &client) && EqualRect(&client, &logical_client));
        CHECK(IDirectDraw_SetCooperativeLevel(draw, window, DDSCL_NORMAL));
        REQUIRE(GetWindowLongW(window, GWL_STYLE) == old_style);
        REQUIRE(GetWindowRect(window, &restored) && EqualRect(&old_rect, &restored));
        REQUIRE(cached_client_rect(window, &client) && EqualRect(&client, &physical_client));
    } else {
        REQUIRE(exercise(draw, 16) == 0);
    }
    REQUIRE(exercise(draw, 32) == 0);
    CHECK(IDirectDraw_RestoreDisplayMode(draw));
    REQUIRE(IDirectDraw_Release(draw) == 0);
    REQUIRE(exercise_interfaces(dll, window) == 0);
    REQUIRE(exercise_compat(dll) == 0);
    REQUIRE(exercise_exports(dll) == 0);
    if (window) DestroyWindow(window);
    FreeLibrary(dll);
    puts("x86 DirectDraw smoke test passed");
    return 0;
}
