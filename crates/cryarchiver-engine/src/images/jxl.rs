use std::fs;
use std::path::Path;

use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{DynamicImage, ImageEncoder};
use tokio_util::sync::CancellationToken;

use crate::container::jxl::{self, JxlMetadata};
use crate::images::decode::{IMAGE_DECODE_ERRORS, SourceLayout};
use crate::images::plan::JxlSettings;
use crate::images::props::Effective;
use crate::metadata::OutputMetadata;
use crate::metadata::icc::Cicp;
use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::video::run_ffmpeg;

const FANCY_UPSAMPLING_FLAGS: &str = "bilinear+full_chroma_int+accurate_rnd";
/// The signature and the IHDR chunk, which is always first.
const PNG_AFTER_IHDR: usize = 8 + 4 + 4 + 13 + 4;
const DROP_EXIF_SIDE_DATA: &str = "sidedata=mode=delete:type=31";
const DROP_DISPLAY_MATRIX: &str = "sidedata=mode=delete:type=DISPLAYMATRIX";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JxlPixels {
	Rgb8,
	Rgba8,
	Rgb16,
	Rgba16,
	Grey8,
	GreyAlpha8,
	Grey16,
	GreyAlpha16,
}

impl JxlPixels {
	pub fn for_source(effective: Effective, grey: bool) -> Self {
		match (grey, effective.high_depth, effective.alpha) {
			(false, false, false) => Self::Rgb8,
			(false, false, true) => Self::Rgba8,
			(false, true, false) => Self::Rgb16,
			(false, true, true) => Self::Rgba16,
			(true, false, false) => Self::Grey8,
			(true, false, true) => Self::GreyAlpha8,
			(true, true, false) => Self::Grey16,
			(true, true, true) => Self::GreyAlpha16,
		}
	}

	fn encoder_format(self) -> &'static str {
		match self {
			Self::Rgb8 => "rgb24",
			Self::Rgba8 => "rgba",
			Self::Rgb16 => "rgb48le",
			Self::Rgba16 => "rgba64le",
			Self::Grey8 => "gray",
			Self::GreyAlpha8 => "ya8",
			Self::Grey16 => "gray16le",
			Self::GreyAlpha16 => "ya16le",
		}
	}
}

/// How `upright_png` lays its pixels out.
pub const UPRIGHT: SourceLayout = SourceLayout {
	grid: false,
	alpha_item: false,
	orientation: 1,
	fancy_upsampling: false,
};

/// `image` as a PNG in `pixels`' format, for libjxl to encode in place of the source, with the source's ICC profile,
/// or without one its CICP colour (a cICP chunk, which ffmpeg reads).
pub fn upright_png(
	image: &DynamicImage,
	pixels: JxlPixels,
	icc: Option<Vec<u8>>,
	cicp: Option<Cicp>,
) -> Result<Vec<u8>, String> {
	let converted = match pixels {
		JxlPixels::Rgb8 => DynamicImage::ImageRgb8(image.to_rgb8()),
		JxlPixels::Rgba8 => DynamicImage::ImageRgba8(image.to_rgba8()),
		JxlPixels::Rgb16 => DynamicImage::ImageRgb16(image.to_rgb16()),
		JxlPixels::Rgba16 => DynamicImage::ImageRgba16(image.to_rgba16()),
		JxlPixels::Grey8 => DynamicImage::ImageLuma8(image.to_luma8()),
		JxlPixels::GreyAlpha8 => DynamicImage::ImageLumaA8(image.to_luma_alpha8()),
		JxlPixels::Grey16 => DynamicImage::ImageLuma16(image.to_luma16()),
		JxlPixels::GreyAlpha16 => DynamicImage::ImageLumaA16(image.to_luma_alpha16()),
	};

	let mut png = Vec::new();
	let mut encoder = PngEncoder::new_with_quality(&mut png, CompressionType::Fast, PngFilter::Adaptive);
	let tagged_by_cicp = icc.is_none();
	if let Some(icc) = icc {
		encoder.set_icc_profile(icc).map_err(|err| err.to_string())?;
	}
	converted.write_with_encoder(encoder).map_err(|err| err.to_string())?;

	match cicp.filter(|_| tagged_by_cicp) {
		Some(cicp) => Ok(with_cicp(&png, libjxl_cicp(cicp))),
		None => Ok(png),
	}
}

/// `cicp` as libjxl can code it. It codes the BT.601 and BT.2020 transfers (6, 14 and 15) only by the BT.709 curve
/// they share (1), and has nothing for SMPTE 240M or IEC 61966-2-4 (7 and 11), which it would write as sRGB, so those
/// are left unspecified.
pub fn libjxl_cicp(cicp: Cicp) -> Cicp {
	let transfer = match cicp.transfer {
		6 | 14 | 15 => 1,
		7 | 9 | 10 | 11 | 12 => Cicp::UNSPECIFIED.transfer,
		other => other,
	};
	Cicp { transfer, ..cicp }
}

/// A cICP chunk (RGB, full range) right after IHDR, where the PNG specification wants it.
fn with_cicp(png: &[u8], cicp: Cicp) -> Vec<u8> {
	let body = [cicp.primaries, cicp.transfer, 0, 1];
	let kind = *b"cICP";
	let mut chunk = 4_u32.to_be_bytes().to_vec();
	chunk.extend(kind);
	chunk.extend(body);
	chunk.extend(crc32fast::hash(&[kind.as_slice(), &body].concat()).to_be_bytes());

	[&png[..PNG_AFTER_IHDR], &chunk, &png[PNG_AFTER_IHDR..]].concat()
}

#[derive(Debug, Clone, Copy)]
pub struct JxlJob<'a> {
	pub source: &'a Path,
	pub layout: SourceLayout,
	pub settings: JxlSettings,
	pub pixels: JxlPixels,
	/// libjxl's threads; without a cap it starts one for every core.
	pub threads: usize,
}

pub fn encode_command(ffmpeg: &Path, job: &JxlJob<'_>, output: &Path) -> ToolCommand {
	let pixel_format = job.pixels.encoder_format();
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.count_lines_containing(IMAGE_DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-y", "-v", "error", "-noautorotate", "-i"])
		.arg(process::tool_path(job.source));
	if job.layout.fancy_upsampling {
		cmd.args(["-sws_flags", FANCY_UPSAMPLING_FLAGS]);
	}

	let filters: Vec<&str> = orientation_filter(job.layout.orientation)
		.into_iter()
		.chain([DROP_EXIF_SIDE_DATA, DROP_DISPLAY_MATRIX])
		.collect();
	if job.layout.alpha_item {
		cmd.arg("-filter_complex").arg(format!(
			"[0:v:0][0:v:1]alphamerge,{},format={pixel_format}",
			filters.join(",")
		));
	} else if job.layout.grid {
		// HEIF and AVIF tile grids are stitched by a complex filtergraph, which a -vf would conflict with.
		cmd.args(["-pix_fmt", pixel_format]);
	} else {
		cmd.arg("-vf").arg(filters.join(",")).args(["-pix_fmt", pixel_format]);
	}

	cmd.args(["-c:v", "libjxl", "-distance"])
		.arg(job.settings.distance.to_string())
		.arg("-effort")
		.arg(job.settings.effort.to_string())
		.arg("-threads")
		.arg(job.threads.max(1).to_string())
		.args(["-update", "1", "-frames:v", "1", "-f", "image2"])
		.arg(output);
	cmd
}

fn orientation_filter(orientation: u16) -> Option<&'static str> {
	match orientation {
		2 => Some("hflip"),
		3 => Some("hflip,vflip"),
		4 => Some("vflip"),
		5 => Some("transpose=cclock_flip"),
		6 => Some("transpose=clock"),
		7 => Some("transpose=clock_flip"),
		8 => Some("transpose=cclock"),
		_ => None,
	}
}

pub async fn encode(
	ffmpeg: &Path,
	job: &JxlJob<'_>,
	scratch: &Path,
	metadata: &OutputMetadata,
	cancel: &CancellationToken,
) -> Result<Vec<u8>, JobError> {
	run_ffmpeg(&encode_command(ffmpeg, job, scratch), |_| {}, cancel).await?;
	let written = fs::read(scratch).map_err(|err| JobError::io(IoSide::Destination(scratch), &err))?;
	let _ = fs::remove_file(scratch);

	let codestream = jxl::extract_codestream(&written)
		.map_err(|err| ErrorKind::EncodeFailed(format!("libjxl wrote no usable codestream: {err}")))?;
	Ok(jxl::write_container(
		&codestream,
		&JxlMetadata {
			exif: metadata.exif.as_deref(),
			xmp: metadata.xmp.as_deref(),
		},
	))
}

#[cfg(test)]
mod tests {
	use image::{GenericImageView, RgbaImage};

	use super::*;

	#[test]
	fn libjxl_runs_on_the_job_threads() {
		let job = JxlJob {
			source: Path::new("in.png"),
			layout: UPRIGHT,
			settings: JxlSettings {
				distance: 1.0,
				effort: 7,
			},
			pixels: JxlPixels::Rgb8,
			threads: 3,
		};

		let args = crate::video::joined(&encode_command(Path::new("ffmpeg"), &job, Path::new("out.jxl")));

		assert!(args.contains("-c:v libjxl -distance 1 -effort 7 -threads 3 "), "{args}");
	}

	#[test]
	fn transfers_libjxl_cannot_code_become_the_same_curve_or_unspecified() {
		let tagged = |transfer| libjxl_cicp(Cicp { primaries: 9, transfer });

		for (transfer, coded) in [
			(6, 1),
			(14, 1),
			(15, 1),
			(7, 2),
			(9, 2),
			(10, 2),
			(11, 2),
			(12, 2),
			(1, 1),
			(13, 13),
			(16, 16),
			(18, 18),
		] {
			assert_eq!(
				tagged(transfer),
				Cicp {
					primaries: 9,
					transfer: coded
				},
				"{transfer}"
			);
		}
	}

	#[test]
	fn an_upright_png_carries_the_profile_or_else_the_cicp_in_the_pixel_format_asked_for() {
		let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 2, image::Rgba([10, 20, 30, 255])));
		let pq = Cicp {
			primaries: 9,
			transfer: 16,
		};
		let profile = moxcms::ColorProfile::new_srgb().encode().unwrap();

		let tagged = upright_png(&image, JxlPixels::Rgb8, None, Some(pq)).unwrap();
		let profiled = upright_png(&image, JxlPixels::Rgb16, Some(profile.clone()), Some(pq)).unwrap();

		let tagged_metadata = crate::metadata::extract(&tagged);
		assert_eq!(tagged_metadata.cicp, Some(pq));
		assert_eq!(tagged_metadata.icc, None);
		let decoded = image::load_from_memory(&tagged).unwrap();
		assert_eq!(decoded.dimensions(), (4, 2));
		assert!(matches!(decoded, DynamicImage::ImageRgb8(_)));

		let profiled_metadata = crate::metadata::extract(&profiled);
		assert_eq!(profiled_metadata.icc, Some(profile));
		assert_eq!(profiled_metadata.cicp, None);
		assert!(matches!(
			image::load_from_memory(&profiled).unwrap(),
			DynamicImage::ImageRgb16(_)
		));
	}
}
