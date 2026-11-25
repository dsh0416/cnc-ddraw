//! CPU pixel copies, conversion, and scaling.
#![forbid(unsafe_code)]

use super::{BltOptions, Format, SurfaceData};
use crate::simd_blit::{self, KeyRange};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::{DirectDraw::DDCOLORKEY, Gdi::PALETTEENTRY};

fn pixel_in_key(pixel: &[u8], range: DDCOLORKEY) -> bool {
    let mut word = [0u8; 4];
    word[..pixel.len()].copy_from_slice(pixel);
    (range.dwColorSpaceLowValue..=range.dwColorSpaceHighValue).contains(&u32::from_le_bytes(word))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn blit(
    dst: &mut SurfaceData,
    dest: RECT,
    source: &[u8],
    pitch: usize,
    src_format: Format,
    src_palette: Option<&[PALETTEENTRY; 256]>,
    src: RECT,
    opts: BltOptions,
) {
    let dst_bytes = dst.format.bytes();
    let src_bytes = src_format.bytes();
    let same_format = dst.format == src_format;
    let (dw, dh) = (
        (dest.right - dest.left) as usize,
        (dest.bottom - dest.top) as usize,
    );
    let (sw, sh) = (
        (src.right - src.left) as usize,
        (src.bottom - src.top) as usize,
    );
    if same_format && dw == sw && !opts.mirror_lr {
        let sk = opts.src_key.map(|k| KeyRange {
            lo: k.dwColorSpaceLowValue,
            hi: k.dwColorSpaceHighValue,
        });
        let dk = opts.dst_key.map(|k| KeyRange {
            lo: k.dwColorSpaceLowValue,
            hi: k.dwColorSpaceHighValue,
        });
        let dst_pitch = dst.storage.pitch;
        let dst_pixels = dst.pixel_slice_mut();
        simd_blit::blit_rect_same_format(
            dst_pixels,
            dst_pitch,
            dest.left as usize,
            dest.top as usize,
            source,
            pitch,
            src.left as usize,
            src.top as usize,
            dw,
            dh,
            sh,
            opts.mirror_ud,
            dst_bytes,
            sk,
            dk,
        );
        return;
    }
    // Fast 16.16 fixed-point DDA stretch blit for same-format non-keyed copies, with row reuse.
    if same_format && opts.src_key.is_none() && opts.dst_key.is_none() && !opts.mirror_lr {
        let dst_pitch = dst.storage.pitch;
        let dst_pixels = dst.pixel_slice_mut();
        let x_step = ((sw as u64) << 16) / (dw as u64);
        let y_step = if dh == sh {
            0u64
        } else {
            ((sh as u64) << 16) / (dh as u64)
        };
        let row_bytes = dw * dst_bytes;
        let mut y_acc = 0u64;
        let mut prev_sy = usize::MAX;
        for y in 0..dh {
            let src_y_rel = if dh == sh {
                y
            } else {
                let r = (y_acc >> 16) as usize;
                y_acc += y_step;
                r
            };
            let sy = if opts.mirror_ud {
                src.top as usize + (sh - 1 - src_y_rel)
            } else {
                src.top as usize + src_y_rel
            };
            let dst_start = (dest.top as usize + y) * dst_pitch + dest.left as usize * dst_bytes;
            if y > 0 && sy == prev_sy {
                let prev_start = dst_start - dst_pitch;
                dst_pixels.copy_within(prev_start..prev_start + row_bytes, dst_start);
                continue;
            }
            prev_sy = sy;
            let src_start = sy * pitch + src.left as usize * src_bytes;
            let s_row = &source[src_start..src_start + sw * src_bytes];
            let d_row = &mut dst_pixels[dst_start..dst_start + row_bytes];
            match dst_bytes {
                1 => stretch_row(s_row, d_row, x_step),
                2 => stretch_row(
                    s_row.as_chunks::<2>().0,
                    d_row.as_chunks_mut::<2>().0,
                    x_step,
                ),
                _ => stretch_row(
                    s_row.as_chunks::<4>().0,
                    d_row.as_chunks_mut::<4>().0,
                    x_step,
                ),
            }
        }
        return;
    }

    let default_pal = [PALETTEENTRY::default(); 256];
    let pal = src_palette.unwrap_or(&default_pal);
    let dst_format = dst.format;
    let dst_pitch = dst.storage.pitch;
    let dst_pixels = dst.pixel_slice_mut();
    let x_step = ((sw as u64) << 16) / (dw as u64);
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
        let sy = if opts.mirror_ud {
            src.top as usize + (sh - 1 - src_y_rel)
        } else {
            src.top as usize + src_y_rel
        };
        let start = sy * pitch + src.left as usize * src_bytes;
        let row = &source[start..start + sw * src_bytes];
        let d_start = (dest.top as usize + y) * dst_pitch + dest.left as usize * dst_bytes;
        let out = &mut dst_pixels[d_start..d_start + dw * dst_bytes];
        let mut x_acc = 0u64;
        for target in out.chunks_exact_mut(dst_bytes) {
            let src_x_rel = (x_acc >> 16) as usize;
            x_acc += x_step;
            let sx = if opts.mirror_lr {
                (sw - 1 - src_x_rel) * src_bytes
            } else {
                src_x_rel * src_bytes
            };
            let pixel = &row[sx..sx + src_bytes];
            if let Some(range) = opts.src_key {
                if pixel_in_key(pixel, range) {
                    continue;
                }
            }
            if let Some(range) = opts.dst_key {
                if !pixel_in_key(target, range) {
                    continue;
                }
            }
            if same_format {
                target.copy_from_slice(pixel);
            } else {
                let rgb = src_format.decode_rgb(pixel, pal);
                dst_format.encode_rgb(rgb, target);
            }
        }
    }
}

fn stretch_row<T: Copy>(src: &[T], dst: &mut [T], step: u64) {
    let mut offset = 0u64;
    for pixel in dst {
        *pixel = src[(offset >> 16) as usize];
        offset += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_downsample_can_reach_beyond_65535_source_pixels() {
        let mut src = vec![0u8; 131_074];
        src[65_537] = 99;
        let mut dst = [0; 2];
        stretch_row(&src, &mut dst, (65_537u64) << 16);
        assert_eq!(dst, [0, 99]);
    }
}
