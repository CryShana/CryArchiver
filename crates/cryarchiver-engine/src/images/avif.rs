use std::borrow::Cow;

use image::{DynamicImage, GenericImageView, RgbImage, RgbaImage};
use ravif::{AlphaColorMode, BitDepth, Encoder, Img, RGB8, RGBA8};

use crate::container::avif::{self, AvifMetadata};
use crate::images::plan::RavifSettings;
use crate::images::props;
use crate::metadata::OutputMetadata;
use crate::metadata::icc::{self, Cicp};

#[derive(Debug, Clone, Copy)]
pub struct AvifColour<'a> {
	pub icc: Option<&'a [u8]>,
	pub cicp: Option<Cicp>,
}

pub fn output_cicp(colour: AvifColour<'_>) -> Cicp {
	avif::writable_cicp(match colour.icc {
		Some(profile) => icc::cicp_for(profile),
		None => colour.cicp.unwrap_or(Cicp::SRGB),
	})
}

pub fn encode(image: &DynamicImage, alpha: bool, settings: RavifSettings, threads: usize) -> Result<Vec<u8>, String> {
	let pool = rayon::ThreadPoolBuilder::new()
		.num_threads(threads.max(1))
		.build()
		.map_err(|err| format!("could not start the encoder threads: {err}"))?;

	pool.install(|| {
		let encoder = Encoder::new()
			.with_quality(settings.quality)
			.with_alpha_quality(settings.quality)
			.with_speed(settings.speed)
			.with_bit_depth(BitDepth::Ten)
			.with_alpha_color_mode(AlphaColorMode::UnassociatedClean)
			.with_num_threads(None);

		let (width, height) = image.dimensions();
		let (width, height) = (width as usize, height as usize);

		let result = if alpha {
			let rgba_image: Cow<'_, RgbaImage> = match image {
				DynamicImage::ImageRgba8(buffer) => Cow::Borrowed(buffer),
				_ => Cow::Owned(props::rgba8(image)),
			};
			let pixels: &[RGBA8] = rgb::bytemuck::cast_slice(rgba_image.as_raw());
			encoder.encode_rgba(Img::new(pixels, width, height))
		} else {
			let rgb_image: Cow<'_, RgbImage> = match image {
				DynamicImage::ImageRgb8(buffer) => Cow::Borrowed(buffer),
				_ => Cow::Owned(props::rgb8(image)),
			};
			let pixels: &[RGB8] = rgb::bytemuck::cast_slice(rgb_image.as_raw());
			encoder.encode_rgb(Img::new(pixels, width, height))
		};

		result
			.map(|encoded| encoded.avif_file)
			.map_err(|err| format!("ravif failed: {err}"))
	})
}

pub fn finish(encoded: &[u8], colour: AvifColour<'_>, metadata: &OutputMetadata) -> Result<Vec<u8>, String> {
	avif::rewrite(
		encoded,
		output_cicp(colour),
		&AvifMetadata {
			icc: colour.icc,
			exif: metadata.exif.as_deref(),
			xmp: metadata.xmp.as_deref(),
		},
		None,
	)
	.map_err(|err| err.to_string())
}
