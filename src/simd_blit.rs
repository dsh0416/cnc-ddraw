//! CPU-side SIMD-accelerated blitting and color-fill kernels powered by `fearless_simd`.
//! Used for pure CPU-backed DirectDraw surface operations (color-keyed blits and fills)
//! that cannot be offloaded to GPU shaders because games freely `Lock()` surface memory.
#![forbid(unsafe_code)]
use fearless_simd::{dispatch, prelude::*, Level, Simd};

#[derive(Clone, Copy, Debug)]
pub struct KeyRange {
    pub lo: u32,
    pub hi: u32,
}

#[inline(always)]
fn mask_in_range_u8<S: Simd>(
    simd: S,
    v: S::u8s,
    lo: S::u8s,
    hi: S::u8s,
    single: bool,
) -> <S::u8s as SimdBase<S>>::Mask {
    let _ = simd;
    if single {
        v.simd_eq(lo)
    } else {
        v.simd_ge(lo) & v.simd_le(hi)
    }
}

#[inline(always)]
fn mask_in_range_u16<S: Simd>(
    simd: S,
    v: S::u16s,
    lo: S::u16s,
    hi: S::u16s,
    single: bool,
) -> <S::u16s as SimdBase<S>>::Mask {
    let _ = simd;
    if single {
        v.simd_eq(lo)
    } else {
        v.simd_ge(lo) & v.simd_le(hi)
    }
}

#[inline(always)]
fn mask_in_range_u32<S: Simd>(
    simd: S,
    v: S::u32s,
    lo: S::u32s,
    hi: S::u32s,
    single: bool,
) -> <S::u32s as SimdBase<S>>::Mask {
    let _ = simd;
    if single {
        v.simd_eq(lo)
    } else {
        v.simd_ge(lo) & v.simd_le(hi)
    }
}

#[inline(always)]
fn fill_row_u8_simd<S: Simd>(simd: S, dst: &mut [u8], color: u8, dst_key: Option<KeyRange>) {
    let Some(dk) = dst_key else {
        dst.fill(color);
        return;
    };
    let (lo, hi) = (dk.lo as u8, dk.hi as u8);
    let single = lo == hi;
    let fill_v = S::u8s::splat(simd, color);
    let lo_v = S::u8s::splat(simd, lo);
    let hi_v = S::u8s::splat(simd, hi);
    let mut chunks = dst.chunks_exact_mut(S::u8s::LEN);
    for chunk in &mut chunks {
        let cur = S::u8s::from_slice(simd, chunk);
        let write = mask_in_range_u8(simd, cur, lo_v, hi_v, single);
        write.select(fill_v, cur).store_slice(chunk);
    }
    for px in chunks.into_remainder() {
        if *px >= lo && *px <= hi {
            *px = color;
        }
    }
}

#[inline(always)]
fn fill_row_u16_simd<S: Simd>(simd: S, dst: &mut [u8], color: u16, dst_key: Option<KeyRange>) {
    let color_bytes = color.to_le_bytes();
    let fill_v = S::u16s::splat(simd, color);
    let byte_lane = S::u8s::LEN;
    match dst_key {
        None => {
            let fill_bytes = fill_v.to_bytes();
            let mut chunks = dst.chunks_exact_mut(byte_lane);
            for chunk in &mut chunks {
                fill_bytes.store_slice(chunk);
            }
            for px in chunks.into_remainder().as_chunks_mut::<2>().0 {
                *px = color_bytes;
            }
        }
        Some(dk) => {
            let (lo, hi) = (dk.lo as u16, dk.hi as u16);
            let single = lo == hi;
            let lo_v = S::u16s::splat(simd, lo);
            let hi_v = S::u16s::splat(simd, hi);
            let mut chunks = dst.chunks_exact_mut(byte_lane);
            for chunk in &mut chunks {
                let cur: S::u16s = S::u8s::from_slice(simd, chunk).bitcast();
                let write = mask_in_range_u16(simd, cur, lo_v, hi_v, single);
                write.select(fill_v, cur).to_bytes().store_slice(chunk);
            }
            for px in chunks.into_remainder().as_chunks_mut::<2>().0 {
                let v = u16::from_le_bytes(*px);
                if v >= lo && v <= hi {
                    *px = color_bytes;
                }
            }
        }
    }
}

#[inline(always)]
fn fill_row_u32_simd<S: Simd>(simd: S, dst: &mut [u8], color: u32, dst_key: Option<KeyRange>) {
    let color_bytes = color.to_le_bytes();
    let fill_v = S::u32s::splat(simd, color);
    let byte_lane = S::u8s::LEN;
    match dst_key {
        None => {
            let fill_bytes = fill_v.to_bytes();
            let mut chunks = dst.chunks_exact_mut(byte_lane);
            for chunk in &mut chunks {
                fill_bytes.store_slice(chunk);
            }
            for px in chunks.into_remainder().as_chunks_mut::<4>().0 {
                *px = color_bytes;
            }
        }
        Some(dk) => {
            let (lo, hi) = (dk.lo, dk.hi);
            let single = lo == hi;
            let lo_v = S::u32s::splat(simd, lo);
            let hi_v = S::u32s::splat(simd, hi);
            let mut chunks = dst.chunks_exact_mut(byte_lane);
            for chunk in &mut chunks {
                let cur: S::u32s = S::u8s::from_slice(simd, chunk).bitcast();
                let write = mask_in_range_u32(simd, cur, lo_v, hi_v, single);
                write.select(fill_v, cur).to_bytes().store_slice(chunk);
            }
            for px in chunks.into_remainder().as_chunks_mut::<4>().0 {
                let v = u32::from_le_bytes(*px);
                if v >= lo && v <= hi {
                    *px = color_bytes;
                }
            }
        }
    }
}

#[inline(always)]
fn blit_row_u8_simd<S: Simd>(
    simd: S,
    src: &[u8],
    dst: &mut [u8],
    src_key: Option<KeyRange>,
    dst_key: Option<KeyRange>,
) {
    match (src_key, dst_key) {
        (None, None) => dst.copy_from_slice(src),
        (Some(sk), None) => {
            let (lo, hi) = (sk.lo as u8, sk.hi as u8);
            let single = lo == hi;
            let lo_v = S::u8s::splat(simd, lo);
            let hi_v = S::u8s::splat(simd, hi);
            let mut s_chunks = src.chunks_exact(S::u8s::LEN);
            let mut d_chunks = dst.chunks_exact_mut(S::u8s::LEN);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv = S::u8s::from_slice(simd, sc);
                let dv = S::u8s::from_slice(simd, dc);
                let skip = mask_in_range_u8(simd, sv, lo_v, hi_v, single);
                skip.select(dv, sv).store_slice(dc);
            }
            for (&sp, dp) in s_chunks.remainder().iter().zip(d_chunks.into_remainder()) {
                if sp < lo || sp > hi {
                    *dp = sp;
                }
            }
        }
        (None, Some(dk)) => {
            let (lo, hi) = (dk.lo as u8, dk.hi as u8);
            let single = lo == hi;
            let lo_v = S::u8s::splat(simd, lo);
            let hi_v = S::u8s::splat(simd, hi);
            let mut s_chunks = src.chunks_exact(S::u8s::LEN);
            let mut d_chunks = dst.chunks_exact_mut(S::u8s::LEN);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv = S::u8s::from_slice(simd, sc);
                let dv = S::u8s::from_slice(simd, dc);
                let write = mask_in_range_u8(simd, dv, lo_v, hi_v, single);
                write.select(sv, dv).store_slice(dc);
            }
            for (&sp, dp) in s_chunks.remainder().iter().zip(d_chunks.into_remainder()) {
                if *dp >= lo && *dp <= hi {
                    *dp = sp;
                }
            }
        }
        (Some(sk), Some(dk)) => {
            let (slo, shi) = (sk.lo as u8, sk.hi as u8);
            let (dlo, dhi) = (dk.lo as u8, dk.hi as u8);
            let s_single = slo == shi;
            let d_single = dlo == dhi;
            let slo_v = S::u8s::splat(simd, slo);
            let shi_v = S::u8s::splat(simd, shi);
            let dlo_v = S::u8s::splat(simd, dlo);
            let dhi_v = S::u8s::splat(simd, dhi);
            let mut s_chunks = src.chunks_exact(S::u8s::LEN);
            let mut d_chunks = dst.chunks_exact_mut(S::u8s::LEN);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv = S::u8s::from_slice(simd, sc);
                let dv = S::u8s::from_slice(simd, dc);
                let s_skip = mask_in_range_u8(simd, sv, slo_v, shi_v, s_single);
                let d_ok = mask_in_range_u8(simd, dv, dlo_v, dhi_v, d_single);
                let write = (!s_skip) & d_ok;
                write.select(sv, dv).store_slice(dc);
            }
            for (&sp, dp) in s_chunks.remainder().iter().zip(d_chunks.into_remainder()) {
                if (sp < slo || sp > shi) && (*dp >= dlo && *dp <= dhi) {
                    *dp = sp;
                }
            }
        }
    }
}

#[inline(always)]
fn blit_row_u16_simd<S: Simd>(
    simd: S,
    src: &[u8],
    dst: &mut [u8],
    src_key: Option<KeyRange>,
    dst_key: Option<KeyRange>,
) {
    let byte_lane = S::u8s::LEN;
    match (src_key, dst_key) {
        (None, None) => dst.copy_from_slice(src),
        (Some(sk), None) => {
            let (lo, hi) = (sk.lo as u16, sk.hi as u16);
            let single = lo == hi;
            let lo_v = S::u16s::splat(simd, lo);
            let hi_v = S::u16s::splat(simd, hi);
            let mut s_chunks = src.chunks_exact(byte_lane);
            let mut d_chunks = dst.chunks_exact_mut(byte_lane);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv: S::u16s = S::u8s::from_slice(simd, sc).bitcast();
                let dv: S::u16s = S::u8s::from_slice(simd, dc).bitcast();
                let skip = mask_in_range_u16(simd, sv, lo_v, hi_v, single);
                skip.select(dv, sv).to_bytes().store_slice(dc);
            }
            for (sp, dp) in s_chunks
                .remainder()
                .as_chunks::<2>()
                .0
                .iter()
                .zip(d_chunks.into_remainder().as_chunks_mut::<2>().0.iter_mut())
            {
                let v = u16::from_le_bytes(*sp);
                if v < lo || v > hi {
                    *dp = *sp;
                }
            }
        }
        (None, Some(dk)) => {
            let (lo, hi) = (dk.lo as u16, dk.hi as u16);
            let single = lo == hi;
            let lo_v = S::u16s::splat(simd, lo);
            let hi_v = S::u16s::splat(simd, hi);
            let mut s_chunks = src.chunks_exact(byte_lane);
            let mut d_chunks = dst.chunks_exact_mut(byte_lane);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv: S::u16s = S::u8s::from_slice(simd, sc).bitcast();
                let dv: S::u16s = S::u8s::from_slice(simd, dc).bitcast();
                let write = mask_in_range_u16(simd, dv, lo_v, hi_v, single);
                write.select(sv, dv).to_bytes().store_slice(dc);
            }
            for (sp, dp) in s_chunks
                .remainder()
                .as_chunks::<2>()
                .0
                .iter()
                .zip(d_chunks.into_remainder().as_chunks_mut::<2>().0.iter_mut())
            {
                let v = u16::from_le_bytes(*dp);
                if v >= lo && v <= hi {
                    *dp = *sp;
                }
            }
        }
        (Some(sk), Some(dk)) => {
            let (slo, shi) = (sk.lo as u16, sk.hi as u16);
            let (dlo, dhi) = (dk.lo as u16, dk.hi as u16);
            let s_single = slo == shi;
            let d_single = dlo == dhi;
            let slo_v = S::u16s::splat(simd, slo);
            let shi_v = S::u16s::splat(simd, shi);
            let dlo_v = S::u16s::splat(simd, dlo);
            let dhi_v = S::u16s::splat(simd, dhi);
            let mut s_chunks = src.chunks_exact(byte_lane);
            let mut d_chunks = dst.chunks_exact_mut(byte_lane);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv: S::u16s = S::u8s::from_slice(simd, sc).bitcast();
                let dv: S::u16s = S::u8s::from_slice(simd, dc).bitcast();
                let s_skip = mask_in_range_u16(simd, sv, slo_v, shi_v, s_single);
                let d_ok = mask_in_range_u16(simd, dv, dlo_v, dhi_v, d_single);
                let write = (!s_skip) & d_ok;
                write.select(sv, dv).to_bytes().store_slice(dc);
            }
            for (sp, dp) in s_chunks
                .remainder()
                .as_chunks::<2>()
                .0
                .iter()
                .zip(d_chunks.into_remainder().as_chunks_mut::<2>().0.iter_mut())
            {
                let sv = u16::from_le_bytes(*sp);
                let dv = u16::from_le_bytes(*dp);
                if (sv < slo || sv > shi) && (dv >= dlo && dv <= dhi) {
                    *dp = *sp;
                }
            }
        }
    }
}

#[inline(always)]
fn blit_row_u32_simd<S: Simd>(
    simd: S,
    src: &[u8],
    dst: &mut [u8],
    src_key: Option<KeyRange>,
    dst_key: Option<KeyRange>,
) {
    let byte_lane = S::u8s::LEN;
    match (src_key, dst_key) {
        (None, None) => dst.copy_from_slice(src),
        (Some(sk), None) => {
            let (lo, hi) = (sk.lo, sk.hi);
            let single = lo == hi;
            let lo_v = S::u32s::splat(simd, lo);
            let hi_v = S::u32s::splat(simd, hi);
            let mut s_chunks = src.chunks_exact(byte_lane);
            let mut d_chunks = dst.chunks_exact_mut(byte_lane);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv: S::u32s = S::u8s::from_slice(simd, sc).bitcast();
                let dv: S::u32s = S::u8s::from_slice(simd, dc).bitcast();
                let skip = mask_in_range_u32(simd, sv, lo_v, hi_v, single);
                skip.select(dv, sv).to_bytes().store_slice(dc);
            }
            for (sp, dp) in s_chunks
                .remainder()
                .as_chunks::<4>()
                .0
                .iter()
                .zip(d_chunks.into_remainder().as_chunks_mut::<4>().0.iter_mut())
            {
                let v = u32::from_le_bytes(*sp);
                if v < lo || v > hi {
                    *dp = *sp;
                }
            }
        }
        (None, Some(dk)) => {
            let (lo, hi) = (dk.lo, dk.hi);
            let single = lo == hi;
            let lo_v = S::u32s::splat(simd, lo);
            let hi_v = S::u32s::splat(simd, hi);
            let mut s_chunks = src.chunks_exact(byte_lane);
            let mut d_chunks = dst.chunks_exact_mut(byte_lane);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv: S::u32s = S::u8s::from_slice(simd, sc).bitcast();
                let dv: S::u32s = S::u8s::from_slice(simd, dc).bitcast();
                let write = mask_in_range_u32(simd, dv, lo_v, hi_v, single);
                write.select(sv, dv).to_bytes().store_slice(dc);
            }
            for (sp, dp) in s_chunks
                .remainder()
                .as_chunks::<4>()
                .0
                .iter()
                .zip(d_chunks.into_remainder().as_chunks_mut::<4>().0.iter_mut())
            {
                let v = u32::from_le_bytes(*dp);
                if v >= lo && v <= hi {
                    *dp = *sp;
                }
            }
        }
        (Some(sk), Some(dk)) => {
            let (slo, shi) = (sk.lo, sk.hi);
            let (dlo, dhi) = (dk.lo, dk.hi);
            let s_single = slo == shi;
            let d_single = dlo == dhi;
            let slo_v = S::u32s::splat(simd, slo);
            let shi_v = S::u32s::splat(simd, shi);
            let dlo_v = S::u32s::splat(simd, dlo);
            let dhi_v = S::u32s::splat(simd, dhi);
            let mut s_chunks = src.chunks_exact(byte_lane);
            let mut d_chunks = dst.chunks_exact_mut(byte_lane);
            for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
                let sv: S::u32s = S::u8s::from_slice(simd, sc).bitcast();
                let dv: S::u32s = S::u8s::from_slice(simd, dc).bitcast();
                let s_skip = mask_in_range_u32(simd, sv, slo_v, shi_v, s_single);
                let d_ok = mask_in_range_u32(simd, dv, dlo_v, dhi_v, d_single);
                let write = (!s_skip) & d_ok;
                write.select(sv, dv).to_bytes().store_slice(dc);
            }
            for (sp, dp) in s_chunks
                .remainder()
                .as_chunks::<4>()
                .0
                .iter()
                .zip(d_chunks.into_remainder().as_chunks_mut::<4>().0.iter_mut())
            {
                let sv = u32::from_le_bytes(*sp);
                let dv = u32::from_le_bytes(*dp);
                if (sv < slo || sv > shi) && (dv >= dlo && dv <= dhi) {
                    *dp = *sp;
                }
            }
        }
    }
}

/// Fills a rectangle `[left..right, top..bottom)` in a CPU surface using runtime-dispatched SIMD.
#[allow(clippy::too_many_arguments)]
pub fn colorfill_rect(
    pixels: &mut [u8],
    pitch: usize,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    bytes: usize,
    color_le: [u8; 4],
    dst_key: Option<KeyRange>,
) {
    let row_bytes = width * bytes;
    if dst_key.is_none() && bytes == 1 {
        let color = color_le[0];
        if left == 0 && row_bytes == pitch {
            let start = top * pitch;
            pixels[start..start + height * pitch].fill(color);
        } else {
            for y in 0..height {
                let start = (top + y) * pitch + left;
                pixels[start..start + row_bytes].fill(color);
            }
        }
        return;
    }
    let level = Level::new();
    match bytes {
        1 => dispatch!(level, simd => {
            let color = color_le[0];
            for y in 0..height {
                let start = (top + y) * pitch + left;
                fill_row_u8_simd(simd, &mut pixels[start..start + row_bytes], color, dst_key);
            }
        }),
        2 => dispatch!(level, simd => {
            let color = u16::from_le_bytes([color_le[0], color_le[1]]);
            if dst_key.is_none() && left == 0 && row_bytes == pitch {
                let start = top * pitch;
                fill_row_u16_simd(simd, &mut pixels[start..start + height * pitch], color, None);
            } else {
                for y in 0..height {
                    let start = (top + y) * pitch + left * 2;
                    fill_row_u16_simd(simd, &mut pixels[start..start + row_bytes], color, dst_key);
                }
            }
        }),
        _ => dispatch!(level, simd => {
            let color = u32::from_le_bytes(color_le);
            if dst_key.is_none() && left == 0 && row_bytes == pitch {
                let start = top * pitch;
                fill_row_u32_simd(simd, &mut pixels[start..start + height * pitch], color, None);
            } else {
                for y in 0..height {
                    let start = (top + y) * pitch + left * 4;
                    fill_row_u32_simd(simd, &mut pixels[start..start + row_bytes], color, dst_key);
                }
            }
        }),
    }
}

#[inline(always)]
fn copy_row_simd<S: Simd>(simd: S, src: &[u8], dst: &mut [u8]) {
    let mut s_chunks = src.chunks_exact(S::u8s::LEN);
    let mut d_chunks = dst.chunks_exact_mut(S::u8s::LEN);
    for (sc, dc) in (&mut s_chunks).zip(&mut d_chunks) {
        S::u8s::from_slice(simd, sc).store_slice(dc);
    }
    let sr = s_chunks.remainder();
    let dr = d_chunks.into_remainder();
    if !sr.is_empty() {
        dr.copy_from_slice(sr);
    }
}

/// Copies a contiguous byte slice using runtime-dispatched SIMD (AVX2 / SSE).
pub fn copy_bytes(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    let level = Level::new();
    dispatch!(level, simd => copy_row_simd(simd, src, dst));
}

/// Blits an unstretched-X, same-format, non-horizontally-mirrored rectangle with optional
/// source and/or destination color keys using runtime-dispatched SIMD.
#[allow(clippy::too_many_arguments)]
pub fn blit_rect_same_format(
    dst_pixels: &mut [u8],
    dst_pitch: usize,
    dst_left: usize,
    dst_top: usize,
    src_pixels: &[u8],
    src_pitch: usize,
    src_left: usize,
    src_top: usize,
    width: usize,
    dh: usize,
    sh: usize,
    mirror_ud: bool,
    bytes: usize,
    src_key: Option<KeyRange>,
    dst_key: Option<KeyRange>,
) {
    let row_bytes = width * bytes;
    let level = Level::new();
    if src_key.is_none() && dst_key.is_none() {
        dispatch!(level, simd => {
            if !mirror_ud
                && dh == sh
                && dst_left == 0
                && src_left == 0
                && row_bytes == dst_pitch
                && dst_pitch == src_pitch
            {
                let total = dh * dst_pitch;
                let s_start = src_top * src_pitch;
                let d_start = dst_top * dst_pitch;
                copy_row_simd(
                    simd,
                    &src_pixels[s_start..s_start + total],
                    &mut dst_pixels[d_start..d_start + total],
                );
                return;
            }
            let y_step = if dh == sh {
                0u64
            } else {
                ((sh as u64) << 16) / (dh as u64)
            };
            let mut y_acc = 0u64;
            for y in 0..dh {
                let src_y_rel = if dh == sh {
                    y
                } else {
                    let r = (y_acc >> 16) as usize;
                    y_acc += y_step;
                    r
                };
                let sy = if mirror_ud {
                    src_top + (sh - 1 - src_y_rel)
                } else {
                    src_top + src_y_rel
                };
                let s_start = sy * src_pitch + src_left * bytes;
                let d_start = (dst_top + y) * dst_pitch + dst_left * bytes;
                copy_row_simd(
                    simd,
                    &src_pixels[s_start..s_start + row_bytes],
                    &mut dst_pixels[d_start..d_start + row_bytes],
                );
            }
        });
        return;
    }
    match bytes {
        1 => dispatch!(level, simd => {
            for y in 0..dh {
                let src_y_rel = if dh == sh { y } else { (y as u64 * sh as u64 / dh as u64) as usize };
                let sy = if mirror_ud { src_top + (sh - 1 - src_y_rel) } else { src_top + src_y_rel };
                let s_start = sy * src_pitch + src_left;
                let d_start = (dst_top + y) * dst_pitch + dst_left;
                blit_row_u8_simd(
                    simd,
                    &src_pixels[s_start..s_start + row_bytes],
                    &mut dst_pixels[d_start..d_start + row_bytes],
                    src_key,
                    dst_key,
                );
            }
        }),
        2 => dispatch!(level, simd => {
            for y in 0..dh {
                let src_y_rel = if dh == sh { y } else { (y as u64 * sh as u64 / dh as u64) as usize };
                let sy = if mirror_ud { src_top + (sh - 1 - src_y_rel) } else { src_top + src_y_rel };
                let s_start = sy * src_pitch + src_left * 2;
                let d_start = (dst_top + y) * dst_pitch + dst_left * 2;
                blit_row_u16_simd(
                    simd,
                    &src_pixels[s_start..s_start + row_bytes],
                    &mut dst_pixels[d_start..d_start + row_bytes],
                    src_key,
                    dst_key,
                );
            }
        }),
        _ => dispatch!(level, simd => {
            for y in 0..dh {
                let src_y_rel = if dh == sh { y } else { (y as u64 * sh as u64 / dh as u64) as usize };
                let sy = if mirror_ud { src_top + (sh - 1 - src_y_rel) } else { src_top + src_y_rel };
                let s_start = sy * src_pitch + src_left * 4;
                let d_start = (dst_top + y) * dst_pitch + dst_left * 4;
                blit_row_u32_simd(
                    simd,
                    &src_pixels[s_start..s_start + row_bytes],
                    &mut dst_pixels[d_start..d_start + row_bytes],
                    src_key,
                    dst_key,
                );
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar_fill(dst: &mut [u8], bytes: usize, color: [u8; 4], dst_key: Option<KeyRange>) {
        for px in dst.chunks_exact_mut(bytes) {
            if let Some(dk) = dst_key {
                let mut w = [0u8; 4];
                w[..bytes].copy_from_slice(px);
                let v = u32::from_le_bytes(w);
                if v < dk.lo || v > dk.hi {
                    continue;
                }
            }
            px.copy_from_slice(&color[..bytes]);
        }
    }

    fn scalar_blit(
        src: &[u8],
        dst: &mut [u8],
        bytes: usize,
        src_key: Option<KeyRange>,
        dst_key: Option<KeyRange>,
    ) {
        for (sp, dp) in src.chunks_exact(bytes).zip(dst.chunks_exact_mut(bytes)) {
            if let Some(sk) = src_key {
                let mut w = [0u8; 4];
                w[..bytes].copy_from_slice(sp);
                let v = u32::from_le_bytes(w);
                if v >= sk.lo && v <= sk.hi {
                    continue;
                }
            }
            if let Some(dk) = dst_key {
                let mut w = [0u8; 4];
                w[..bytes].copy_from_slice(dp);
                let v = u32::from_le_bytes(w);
                if v < dk.lo || v > dk.hi {
                    continue;
                }
            }
            dp.copy_from_slice(sp);
        }
    }

    #[test]
    fn simd_colorfill_matches_scalar_across_widths_and_keys() {
        for bytes in [1usize, 2, 4] {
            let width = 73; // Exercises AVX2/SSE vectors + unaligned tail.
            let pitch = ((width + 3) * bytes + 3) & !3;
            let color = [0x12, 0x34, 0x56, 0x78];
            let keys = [
                None,
                Some(KeyRange { lo: 5, hi: 5 }),
                Some(KeyRange { lo: 3, hi: 19 }),
            ];
            for dk in keys {
                let mut simd_buf = vec![0u8; pitch * 3];
                for (i, b) in simd_buf.iter_mut().enumerate() {
                    *b = ((i * 7 + 3) & 0x1f) as u8;
                }
                let mut ref_buf = simd_buf.clone();
                colorfill_rect(&mut simd_buf, pitch, 1, 1, width, 2, bytes, color, dk);
                for y in 1..3 {
                    let s = y * pitch + bytes;
                    scalar_fill(&mut ref_buf[s..s + width * bytes], bytes, color, dk);
                }
                assert_eq!(
                    simd_buf, ref_buf,
                    "colorfill mismatch for bytes={bytes} dk={dk:?}"
                );
            }
        }
    }

    #[test]
    fn simd_keyed_blit_matches_scalar_across_formats_and_key_combinations() {
        for bytes in [1usize, 2, 4] {
            let width = 77;
            let pitch = ((width + 5) * bytes + 3) & !3;
            let mut src = vec![0u8; pitch * 4];
            for (i, b) in src.iter_mut().enumerate() {
                *b = ((i * 11 + 1) & 0x3f) as u8;
            }
            let key_opts = [
                None,
                Some(KeyRange { lo: 7, hi: 7 }),
                Some(KeyRange { lo: 4, hi: 24 }),
            ];
            for sk in key_opts {
                for dk in key_opts {
                    let mut simd_dst = vec![0u8; pitch * 4];
                    for (i, b) in simd_dst.iter_mut().enumerate() {
                        *b = ((i * 5 + 9) & 0x3f) as u8;
                    }
                    let mut ref_dst = simd_dst.clone();
                    blit_rect_same_format(
                        &mut simd_dst,
                        pitch,
                        3,
                        1,
                        &src,
                        pitch,
                        1,
                        0,
                        width,
                        2,
                        2,
                        false,
                        bytes,
                        sk,
                        dk,
                    );
                    for y in 0..2 {
                        let ss = y * pitch + bytes;
                        let ds = (1 + y) * pitch + 3 * bytes;
                        scalar_blit(
                            &src[ss..ss + width * bytes],
                            &mut ref_dst[ds..ds + width * bytes],
                            bytes,
                            sk,
                            dk,
                        );
                    }
                    assert_eq!(
                        simd_dst, ref_dst,
                        "blit mismatch for bytes={bytes} sk={sk:?} dk={dk:?}"
                    );
                }
            }
        }
    }
}
