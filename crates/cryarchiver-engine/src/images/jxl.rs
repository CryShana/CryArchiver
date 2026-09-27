use std::fs;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::container::jxl::{self, JxlMetadata};
use crate::images::decode::{IMAGE_DECODE_ERRORS, SourceLayout};
use crate::images::plan::JxlSettings;
use crate::images::props::Effective;
use crate::metadata::OutputMetadata;
use crate::outcome::{ErrorKind, IoSide, JobError};
use crate::process::{self, STALL_TIMEOUT, ToolCommand};
use crate::video::run_ffmpeg;

const FANCY_UPSAMPLING_FLAGS: &str = "bilinear+full_chroma_int+accurate_rnd";
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

#[derive(Debug, Clone, Copy)]
pub struct JxlJob<'a> {
	pub source: &'a Path,
	pub layout: SourceLayout,
	pub settings: JxlSettings,
	pub pixels: JxlPixels,
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
