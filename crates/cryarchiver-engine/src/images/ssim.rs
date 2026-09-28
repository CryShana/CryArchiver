use std::borrow::Cow;

use fast_ssim2::{LinearRgbImage, MAX_IMAGE_PIXELS, compute_ssimulacra2_strip, srgb_to_linear};
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgba};
use jxl_oxide::{EnumColourEncoding, JxlImage, Moxcms, RenderingIntent};
use moxcms::{ColorProfile, Layout, TransformOptions};

use crate::metadata::icc::{self, StandardSpace};

const STRIP_HEIGHT: u32 = 256;
const FULL_SCALES_SIDE: u32 = 256;
const BLACK: f32 = 0.0;
const WHITE: f32 = 65535.0;

type Rgba16 = ImageBuffer<Rgba<u16>, Vec<u16>>;

pub fn score(reference: &DynamicImage, distorted: &DynamicImage) -> Result<f64, String> {
	if reference.dimensions() != distorted.dimensions() {
		return Err(format!(
			"the output is {:?} but the source is {:?}",
			distorted.dimensions(),
			reference.dimensions()
		));
	}

	let (reference, distorted) = (reference.to_rgba16(), distorted.to_rgba16());
	let transparent = |image: &Rgba16| image.pixels().any(|pixel| pixel[3] != u16::MAX);
	if !transparent(&reference) && !transparent(&distorted) {
		return compare(&reference, &distorted, BLACK);
	}
	Ok(compare(&reference, &distorted, BLACK)?.min(compare(&reference, &distorted, WHITE)?))
}

fn compare(reference: &Rgba16, distorted: &Rgba16, background: f32) -> Result<f64, String> {
	let (width, height) = padded_size(reference.dimensions());
	if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS as u64 {
		return Err(format!(
			"SSIMULACRA2 failed: a {}×{} image is too narrow for its length to be measured",
			reference.width(),
			reference.height()
		));
	}

	let (reference, distorted) = (linear(reference, background), linear(distorted, background));
	compute_ssimulacra2_strip(reference, distorted, STRIP_HEIGHT).map_err(|err| format!("SSIMULACRA2 failed: {err}"))
}

/// SSIMULACRA2 halves the image until a side drops below 8 pixels, and its weights assume all six scales: with fewer,
/// the fixed weight order shifts onto the wrong channels and scales, and the score can fall far below or rise above
/// what the same content scores at full size. A side shorter than 256 pixels is mirrored out to 256.
pub fn padded_size((width, height): (u32, u32)) -> (u32, u32) {
	(width.max(FULL_SCALES_SIDE), height.max(FULL_SCALES_SIDE))
}

fn linear(image: &Rgba16, background: f32) -> LinearRgbImage {
	let (width, height) = image.dimensions();
	let (padded_width, padded_height) = padded_size((width, height));
	let convert = |pixel: &Rgba<u16>| {
		let alpha = f32::from(pixel[3]) / 65535.0;
		let channel = |value: u16| srgb_to_linear((f32::from(value) * alpha + background * (1.0 - alpha)) / 65535.0);
		[channel(pixel[0]), channel(pixel[1]), channel(pixel[2])]
	};

	let data = if (padded_width, padded_height) == (width, height) {
		image.pixels().map(convert).collect()
	} else {
		(0..padded_height)
			.flat_map(|y| (0..padded_width).map(move |x| (x, y)))
			.map(|(x, y)| convert(image.get_pixel(mirrored(x, width), mirrored(y, height))))
			.collect()
	};
	LinearRgbImage::new(data, padded_width as usize, padded_height as usize)
}

fn mirrored(position: u32, size: u32) -> u32 {
	let period = 2 * size;
	let folded = position % period;
	if folded < size { folded } else { period - 1 - folded }
}

pub fn to_srgb<'a>(image: &'a DynamicImage, icc: Option<&[u8]>) -> Result<Cow<'a, DynamicImage>, String> {
	let Some(profile) = icc.filter(|profile| icc::classify(profile) != Some(StandardSpace::Srgb)) else {
		return Ok(Cow::Borrowed(image));
	};

	let source = ColorProfile::new_from_slice(profile).map_err(|err| format!("unreadable ICC profile: {err:?}"))?;
	let srgb = ColorProfile::new_srgb();
	let rgba = image.to_rgba16();
	let (width, height) = rgba.dimensions();
	let grey = icc::colour_space(profile) == Some(*b"GRAY");

	let (layout, input): (Layout, Vec<u16>) = if grey {
		let pairs = rgba.pixels().flat_map(|pixel| [pixel[0], pixel[3]]).collect();
		(Layout::GrayAlpha, pairs)
	} else {
		(Layout::Rgba, rgba.into_raw())
	};
	let transform = source
		.create_transform_16bit(layout, &srgb, Layout::Rgba, TransformOptions::default())
		.map_err(|err| format!("no transform to sRGB: {err:?}"))?;

	let mut output = vec![0u16; width as usize * height as usize * 4];
	transform
		.transform(&input, &mut output)
		.map_err(|err| format!("the transform to sRGB failed: {err:?}"))?;
	ImageBuffer::from_raw(width, height, output)
		.map(|converted| Cow::Owned(DynamicImage::ImageRgba16(converted)))
		.ok_or_else(|| "the converted image has the wrong size".to_owned())
}

pub fn render_jxl_srgb(bytes: &[u8]) -> Result<DynamicImage, String> {
	let mut image = JxlImage::builder()
		.read(bytes)
		.map_err(|err| format!("jxl-oxide could not read the output: {err}"))?;
	image.set_cms(Moxcms);
	image.request_color_encoding(EnumColourEncoding::srgb(RenderingIntent::Relative));

	let render = image
		.render_frame(0)
		.map_err(|err| format!("jxl-oxide could not decode the output: {err}"))?;
	let mut stream = render.stream();
	let (width, height, channels) = (stream.width(), stream.height(), stream.channels());
	let mut samples = vec![0u16; width as usize * height as usize * channels as usize];
	stream.write_to_buffer(&mut samples);

	let rendered = match channels {
		3 => ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageRgb16),
		4 => ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageRgba16),
		other => return Err(format!("jxl-oxide rendered {other} channels instead of RGB")),
	};
	rendered.ok_or_else(|| "the rendered frame has the wrong size".to_owned())
}
