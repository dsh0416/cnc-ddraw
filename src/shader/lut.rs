//! Pure-Rust PNG loader for RetroArch external LUT textures.
#![forbid(unsafe_code)]

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn load_png_rgba(path: &Path) -> Result<DecodedImage, String> {
    let file =
        File::open(path).map_err(|err| format!("opening LUT image {}: {err}", path.display()))?;
    decode_png_rgba(BufReader::new(file))
        .map_err(|err| format!("decoding LUT image {}: {err}", path.display()))
}

pub fn decode_png_rgba<R: std::io::Read>(reader: R) -> Result<DecodedImage, String> {
    let mut decoder = png::Decoder::new(reader);
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|err| format!("reading PNG header: {err}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|err| format!("decoding PNG frame: {err}"))?;
    let bytes = &buf[..info.buffer_size()];

    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for chunk in bytes.chunks_exact(3) {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for chunk in bytes.chunks_exact(2) {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for &g in bytes {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        other => {
            return Err(format!("unsupported expanded PNG color type: {other:?}"));
        }
    };

    if rgba.len() != (info.width as usize) * (info.height as usize) * 4 {
        return Err(format!(
            "unexpected decoded RGBA length {} for {}x{}",
            rgba.len(),
            info.width,
            info.height
        ));
    }

    Ok(DecodedImage {
        width: info.width,
        height: info.height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rgb_png_to_rgba8() {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[10, 20, 30, 40, 50, 60]).unwrap();
        }
        let img = decode_png_rgba(encoded.as_slice()).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        assert_eq!(img.rgba, vec![10, 20, 30, 255, 40, 50, 60, 255]);
    }
}
