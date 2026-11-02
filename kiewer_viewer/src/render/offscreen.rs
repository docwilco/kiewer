//! A frame drawn into a texture instead of a surface, and read back as
//! pixels: the headless [`Target`] for the golden images and the
//! thumbnails (ADR 0004). An [`Offscreen`] is one RGBA8 texture the
//! renderer draws into through the same [`Renderer::render`] call the
//! browser makes; [`Offscreen::read`] copies it into a buffer, waits for
//! the device, and hands back an [`Image`], which encodes to PNG and
//! decodes from one.
//!
//! The format is [`Offscreen::FORMAT`], RGBA8 without the sRGB flag: the
//! shader then writes sRGB-encoded values and the blend and the
//! multisample resolve work on them, which is how a browser canvas and
//! the hand-off's rasteriser (rsvg) compose a translucent layer, so the
//! mask over the copper comes out the same tint.
//!
//! [`Renderer::render`]: crate::render::Renderer::render

use std::sync::mpsc;

use crate::error::{Error, Result};
use crate::render::Target;

/// Pixels as read back: `width * height` RGBA quadruples, row-major from
/// the top left, with no row padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    /// The pixel at (`x`, `y`) as `[r, g, b, a]`, `None` outside the image.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let start = (y as usize * self.width as usize + x as usize) * 4;
        let bytes = self.rgba.get(start..start + 4)?;
        Some([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    /// The image as a PNG: 8-bit RGBA, at the encoder's default
    /// compression.
    ///
    /// # Errors
    ///
    /// [`Error::PngEncode`] when the pixel count does not match the size.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&self.rgba)?;
        Ok(out)
    }

    /// The image a PNG holds: 8-bit RGBA, or 8-bit RGB given alpha 255.
    ///
    /// # Errors
    ///
    /// [`Error::PngDecode`] for bytes that are not a PNG,
    /// [`Error::PngColorType`] for a PNG in another colour type or depth.
    pub fn from_png(bytes: &[u8]) -> Result<Image> {
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info()?;
        let mut buffer = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut buffer)?;
        buffer.truncate(info.buffer_size());
        if info.bit_depth != png::BitDepth::Eight {
            return Err(Error::PngColorType(info.color_type));
        }
        let rgba = match info.color_type {
            png::ColorType::Rgba => buffer,
            png::ColorType::Rgb => buffer
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255])
                .collect(),
            other => return Err(Error::PngColorType(other)),
        };
        Ok(Image {
            width: info.width,
            height: info.height,
            rgba,
        })
    }
}

/// A texture the renderer draws into and the CPU reads back.
pub struct Offscreen {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl Offscreen {
    /// The texture format: RGBA8, blended and resolved on sRGB-encoded
    /// values (the module doc says why).
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    /// A target of `width` by `height` pixels on `device` (each at least 1).
    #[must_use]
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            width,
            height,
        }
    }

    /// The size in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The target a frame is drawn into, cleared to `clear` first.
    #[must_use]
    pub fn target(&self, clear: wgpu::Color) -> Target<'_> {
        Target {
            view: &self.view,
            size: (self.width, self.height),
            clear,
        }
    }

    /// The pixels as drawn so far: copies the texture into a buffer,
    /// waits for the device to finish everything submitted, and strips
    /// the row padding the copy needs.
    ///
    /// # Errors
    ///
    /// [`Error::Readback`] or [`Error::MappedRange`] when the device will
    /// not map the buffer: a lost device, or one out of memory.
    pub fn read(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Image> {
        let unpadded = self.width * 4;
        let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(padded) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("readback"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
        let (sender, receiver) = mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                // The receiver is below; a send after it dropped is impossible
                // here and would mean nothing anyway.
                let _ = sender.send(result);
            });
        // The map completes when the device has run the copy; Wait blocks
        // until everything submitted has.
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        match receiver.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => return Err(wgpu::BufferAsyncError.into()),
        }
        let rgba = {
            let mapped = buffer.slice(..).get_mapped_range()?;
            let mut rgba = Vec::with_capacity(unpadded as usize * self.height as usize);
            for row in mapped.chunks_exact(padded as usize) {
                rgba.extend_from_slice(&row[..unpadded as usize]);
            }
            rgba
        };
        buffer.unmap();
        Ok(Image {
            width: self.width,
            height: self.height,
            rgba,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> Image {
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the test image is under 256 pixels on a side"
                )]
                rgba.extend([x as u8, y as u8, 128, 255]);
            }
        }
        Image {
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn an_image_round_trips_through_png() {
        let image = gradient(37, 11);
        let png = image.to_png().expect("encodes");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let back = Image::from_png(&png).expect("decodes");
        assert_eq!(back, image);
        assert_eq!(back.pixel(36, 10), Some([36, 10, 128, 255]));
        assert_eq!(back.pixel(37, 0), None);
    }

    #[test]
    fn an_rgb_png_gets_an_opaque_alpha_and_other_depths_are_refused() {
        let mut rgb = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut rgb, 2, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .expect("header")
                .write_image_data(&[1, 2, 3, 4, 5, 6])
                .expect("data");
        }
        let image = Image::from_png(&rgb).expect("decodes");
        assert_eq!(image.rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
        let mut gray = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut gray, 1, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .expect("header")
                .write_image_data(&[7])
                .expect("data");
        }
        let error = Image::from_png(&gray).expect_err("refused");
        assert!(matches!(
            error,
            Error::PngColorType(png::ColorType::Grayscale)
        ));
        assert!(Image::from_png(b"not a png").is_err());
    }
}
