use image::{DynamicImage, RgbImage, RgbaImage};

use crate::metadata::{ImageFormat, sniff};

const PALETTE_COLOURS: u32 = 256;
const FLAT_RUNS: f64 = 0.75;
const FLAT_BLOCKS: f64 = 0.5;
const BLOCK: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
	pub high_depth: bool,
	pub alpha: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphicMetrics {
	pub unique_colours: u32,
	pub equal_right: f64,
	pub flat_blocks: f64,
}

pub fn effective(image: &DynamicImage) -> Effective {
	let high_depth = match image {
		DynamicImage::ImageLuma16(buffer) => !eight_bit(buffer.as_raw()),
		DynamicImage::ImageLumaA16(buffer) => !eight_bit(buffer.as_raw()),
		DynamicImage::ImageRgb16(buffer) => !eight_bit(buffer.as_raw()),
		DynamicImage::ImageRgba16(buffer) => !eight_bit(buffer.as_raw()),
		DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => true,
		_ => false,
	};
	let alpha = match image {
		DynamicImage::ImageLumaA8(buffer) => buffer.pixels().any(|pixel| pixel[1] != u8::MAX),
		DynamicImage::ImageRgba8(buffer) => buffer.pixels().any(|pixel| pixel[3] != u8::MAX),
		DynamicImage::ImageLumaA16(buffer) => buffer.pixels().any(|pixel| pixel[1] != u16::MAX),
		DynamicImage::ImageRgba16(buffer) => buffer.pixels().any(|pixel| pixel[3] != u16::MAX),
		DynamicImage::ImageRgba32F(buffer) => buffer.pixels().any(|pixel| pixel[3] < 1.0),
		_ => false,
	};
	Effective { high_depth, alpha }
}

fn eight_bit(samples: &[u16]) -> bool {
	let replicated = samples.iter().all(|&sample| sample >> 8 == sample & 0xFF);
	replicated || samples.iter().all(|&sample| sample.trailing_zeros() >= 8)
}

pub fn rgba8(image: &DynamicImage) -> RgbaImage {
	if !is_sixteen_bit(image) {
		return image.to_rgba8();
	}

	let wide = image.to_rgba16();
	let (width, height) = wide.dimensions();
	let narrow = wide
		.into_raw()
		.into_iter()
		.map(|sample| sample.to_be_bytes()[0])
		.collect();
	RgbaImage::from_raw(width, height, narrow).expect("same size")
}

pub fn rgb8(image: &DynamicImage) -> RgbImage {
	if !is_sixteen_bit(image) {
		return image.to_rgb8();
	}

	let wide = image.to_rgb16();
	let (width, height) = wide.dimensions();
	let narrow = wide
		.into_raw()
		.into_iter()
		.map(|sample| sample.to_be_bytes()[0])
		.collect();
	RgbImage::from_raw(width, height, narrow).expect("same size")
}

fn is_sixteen_bit(image: &DynamicImage) -> bool {
	matches!(
		image,
		DynamicImage::ImageLuma16(_)
			| DynamicImage::ImageLumaA16(_)
			| DynamicImage::ImageRgb16(_)
			| DynamicImage::ImageRgba16(_)
	)
}

#[allow(clippy::cast_precision_loss)]
pub fn graphic_metrics(rgb: &RgbImage) -> GraphicMetrics {
	let (width, height) = rgb.dimensions();
	let mut seen = vec![0u64; (1 << 24) / 64];
	let mut unique_colours = 0u32;
	let mut equal_right = 0u64;

	for row in rgb.as_raw().chunks_exact(width as usize * 3) {
		let pixels = row.as_chunks::<3>().0;
		for (index, pixel) in pixels.iter().enumerate() {
			let key = (usize::from(pixel[0]) << 16) | (usize::from(pixel[1]) << 8) | usize::from(pixel[2]);
			let bit = 1u64 << (key & 63);
			if seen[key >> 6] & bit == 0 {
				seen[key >> 6] |= bit;
				unique_colours += 1;
			}
			if pixels.get(index + 1) == Some(pixel) {
				equal_right += 1;
			}
		}
	}

	let pixels = u64::from(width) * u64::from(height);
	GraphicMetrics {
		unique_colours,
		equal_right: if pixels == 0 {
			0.0
		} else {
			equal_right as f64 / pixels as f64
		},
		flat_blocks: flat_blocks(rgb),
	}
}

#[allow(clippy::cast_precision_loss)]
fn flat_blocks(rgb: &RgbImage) -> f64 {
	let (width, height) = rgb.dimensions();
	let mut flat = 0u64;
	let mut total = 0u64;

	for top in (0..height.saturating_sub(BLOCK - 1)).step_by(BLOCK as usize) {
		for left in (0..width.saturating_sub(BLOCK - 1)).step_by(BLOCK as usize) {
			let first = rgb.get_pixel(left, top);
			let uniform = (top..top + BLOCK).all(|y| (left..left + BLOCK).all(|x| rgb.get_pixel(x, y) == first));
			total += 1;
			flat += u64::from(uniform);
		}
	}
	if total == 0 { 0.0 } else { flat as f64 / total as f64 }
}

pub fn is_graphic(metrics: &GraphicMetrics, lossy_source: bool) -> bool {
	!lossy_source
		&& (metrics.unique_colours <= PALETTE_COLOURS
			|| (metrics.equal_right >= FLAT_RUNS && metrics.flat_blocks >= FLAT_BLOCKS))
}

pub fn lossy_source(bytes: &[u8]) -> bool {
	match sniff(bytes) {
		ImageFormat::Jpeg | ImageFormat::Heif | ImageFormat::Avif | ImageFormat::Jxl => true,
		ImageFormat::WebP => webp_is_lossy(bytes),
		_ => false,
	}
}

fn webp_is_lossy(bytes: &[u8]) -> bool {
	let mut at = 12;
	while let Some(kind) = bytes.get(at..at + 4) {
		match kind {
			b"VP8 " => return true,
			b"VP8L" => return false,
			_ => {}
		}
		let Some(len) = bytes
			.get(at + 4..at + 8)
			.and_then(|len| usize::try_from(u32::from_le_bytes(len.try_into().ok()?)).ok())
		else {
			return false;
		};
		at += 8 + len + len % 2;
	}
	false
}

#[cfg(test)]
mod tests {
	use image::{ImageBuffer, Rgb, Rgba};

	use super::*;

	fn rgb16(samples: impl Fn(u32, u32) -> u16) -> DynamicImage {
		DynamicImage::ImageRgb16(ImageBuffer::from_fn(16, 16, |x, y| {
			let value = samples(x, y);
			Rgb([value, value, value])
		}))
	}

	#[test]
	fn widened_8_bit_data_counts_as_8_bit() {
		let replicated = rgb16(|x, y| u16::try_from(x * 16 + y).unwrap() * 257);
		let shifted = rgb16(|x, y| u16::try_from(x * 16 + y).unwrap() << 8);
		let gradient = rgb16(|x, y| u16::try_from(x * 4096 + y * 7).unwrap());

		assert!(!effective(&replicated).high_depth);
		assert!(!effective(&shifted).high_depth);
		assert!(effective(&gradient).high_depth);
		assert_eq!(rgb8(&shifted).get_pixel(1, 2).0, [18, 18, 18]);
		assert_eq!(rgba8(&replicated).get_pixel(1, 2).0, [18, 18, 18, 255]);
	}

	#[test]
	fn alpha_counts_only_when_a_pixel_is_not_opaque() {
		let opaque = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(8, 8, Rgba([1, 2, 3, 255])));
		let mut transparent = opaque.to_rgba8();
		transparent.put_pixel(3, 3, Rgba([1, 2, 3, 128]));

		assert!(!effective(&opaque).alpha);
		assert!(effective(&DynamicImage::ImageRgba8(transparent)).alpha);
		assert!(!effective(&DynamicImage::new_rgb8(8, 8)).alpha);
	}

	#[test]
	fn flat_low_colour_images_are_graphics_but_noise_and_lossy_sources_are_not() {
		let flat = ImageBuffer::from_fn(64, 64, |x, y| {
			Rgb([
				u8::try_from((x / 16) * 60).unwrap(),
				u8::try_from((y / 16) * 60).unwrap(),
				0,
			])
		});
		let mut state = 12_345u32;
		let noise = ImageBuffer::from_fn(64, 64, |_, _| {
			state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
			let [a, b, c, _] = state.to_le_bytes();
			Rgb([a, b, c])
		});

		let flat_metrics = graphic_metrics(&flat);

		assert_eq!(flat_metrics.unique_colours, 16);
		assert!(flat_metrics.flat_blocks > 0.99);
		assert!(is_graphic(&flat_metrics, false));
		assert!(!is_graphic(&flat_metrics, true));
		assert!(!is_graphic(&graphic_metrics(&noise), false));
	}

	#[test]
	fn lossy_sources_are_recognised_by_content() {
		assert!(lossy_source(&[0xFF, 0xD8, 0xFF, 0xE0]));
		assert!(lossy_source(
			b"RIFF\x10\0\0\0WEBPVP8X\x0A\0\0\0\0\0\0\0\0\0\0\0\0\0VP8 \x04\0\0\0"
		));
		assert!(!lossy_source(b"RIFF\x10\0\0\0WEBPVP8L\x04\0\0\0\0\0\0\0"));
		assert!(!lossy_source(b"\x89PNG\r\n\x1a\n"));
	}
}
