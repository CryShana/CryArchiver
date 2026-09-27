use std::fmt::Display;
use std::io::Cursor;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;

use image::metadata::Orientation;
use image::{DynamicImage, ImageBuffer, ImageReader};
use jxl_oxide::JxlImage;
use tokio_util::sync::CancellationToken;

use crate::container::isobmff;
use crate::images::blocking;
use crate::metadata::icc::Cicp;
use crate::metadata::{ImageFormat, SourceMetadata, exif, sniff};
use crate::outcome::{ErrorKind, JobError, KeepReason};
use crate::process::{self, STALL_TIMEOUT, StdoutMode, ToolCommand};
use crate::toolchain::Toolchain;

pub const IMAGE_DECODE_ERRORS: &[&str] = &[
	"error count:",
	"error y=",
	"overread",
	"LZW decode failed",
	"inflate returned error",
	"chunk too big",
	"Error parsing OBU data",
	"corrupt input packet",
];

#[derive(Debug, Clone, Copy)]
pub struct PamRequest {
	pub alpha_item: bool,
	pub deep: bool,
	pub keep_stored_orientation: bool,
}

#[derive(Debug, Clone)]
pub struct Decoded {
	pub image: DynamicImage,
	pub icc: Option<Vec<u8>>,
	pub cicp: Option<Cicp>,
	pub decode_errors: usize,
}

#[derive(Debug)]
pub enum Undecodable {
	Keep(KeepReason),
	Failed(JobError),
}

impl From<JobError> for Undecodable {
	fn from(err: JobError) -> Self {
		Self::Failed(err)
	}
}

pub fn header_dimensions(bytes: &[u8], probed: Option<(u32, u32)>) -> Option<(u32, u32)> {
	let from_crate = ImageReader::new(Cursor::new(bytes))
		.with_guessed_format()
		.ok()
		.and_then(|reader| reader.into_dimensions().ok());
	if from_crate.is_some() {
		return from_crate;
	}

	match sniff(bytes) {
		ImageFormat::Heif | ImageFormat::Avif => isobmff::parse_meta(bytes).ok()?.primary_size(),
		ImageFormat::Jxl => JxlImage::builder()
			.read(bytes)
			.ok()
			.map(|image| (image.width(), image.height())),
		_ => probed.filter(|(width, height)| *width > 0 && *height > 0),
	}
}

pub fn orientation(bytes: &[u8], metadata: &SourceMetadata) -> u16 {
	match sniff(bytes) {
		ImageFormat::Heif | ImageFormat::Avif => {
			isobmff::parse_meta(bytes).map_or(1, |meta| meta.primary_orientation())
		}
		ImageFormat::Jxl => 1,
		_ => metadata.exif.as_deref().and_then(exif::orientation).unwrap_or(1),
	}
}

pub async fn decode(
	tools: &Toolchain,
	source: &Path,
	bytes: Arc<Vec<u8>>,
	metadata: &SourceMetadata,
	source_pix_fmt: Option<&str>,
	cancel: &CancellationToken,
) -> Result<Decoded, Undecodable> {
	let format = sniff(&bytes);
	if format == ImageFormat::Jxl {
		return blocking(cancel, move || decode_jxl(&bytes)).await?;
	}
	if format == ImageFormat::Heif && isobmff::parse_meta(&bytes).is_ok_and(|meta| meta.alpha_item().is_some()) {
		return Err(keep("HEIF images with transparency are not supported yet"));
	}

	let orientation = orientation(&bytes, metadata);
	let from_crate = {
		let bytes = Arc::clone(&bytes);
		blocking(cancel, move || decode_with_crate(&bytes)).await?
	};
	let (image, decode_errors) = match from_crate {
		Some(image) if format == ImageFormat::Jpeg => (image, count_decode_errors(tools, source, cancel).await?),
		Some(image) => (image, 0),
		None => {
			let request = PamRequest {
				alpha_item: format == ImageFormat::Avif
					&& isobmff::parse_meta(&bytes).is_ok_and(|meta| meta.alpha_item().is_some()),
				deep: source_pix_fmt.is_some_and(deep_pix_fmt),
				keep_stored_orientation: true,
			};
			run_pam(&pam_command(&tools.ffmpeg, source, request), cancel).await?
		}
	};

	let image = blocking(cancel, move || oriented(image, orientation)).await?;
	Ok(Decoded {
		image,
		icc: metadata.icc.clone(),
		cicp: metadata.cicp,
		decode_errors,
	})
}

fn keep(reason: &str) -> Undecodable {
	Undecodable::Keep(KeepReason::Unsupported(reason.to_owned()))
}

fn damaged(err: impl Display) -> Undecodable {
	Undecodable::Failed(ErrorKind::SourceDamaged(format!("the image could not be decoded: {err}")).into())
}

fn decode_with_crate(bytes: &[u8]) -> Option<DynamicImage> {
	panic::catch_unwind(AssertUnwindSafe(|| {
		let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
		reader.no_limits();
		reader.decode().ok()
	}))
	.ok()
	.flatten()
}

pub fn deep_pix_fmt(pix_fmt: &str) -> bool {
	let packed_low_depth = ["rgb444", "bgr444", "rgb555", "bgr555", "rgb565", "bgr565"]
		.iter()
		.any(|prefix| pix_fmt.starts_with(prefix));
	(pix_fmt.ends_with("le") || pix_fmt.ends_with("be")) && !packed_low_depth
}

pub fn pam_command(ffmpeg: &Path, source: &Path, request: PamRequest) -> ToolCommand {
	let pix_fmt = if request.deep { "rgba64be" } else { "rgba" };
	let mut cmd = ToolCommand::new(ffmpeg, STALL_TIMEOUT);
	cmd.count_lines_containing(IMAGE_DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-v", "error"]);
	if request.keep_stored_orientation {
		cmd.arg("-noautorotate");
	}
	cmd.arg("-i").arg(process::tool_path(source));

	if request.alpha_item {
		cmd.arg("-filter_complex")
			.arg(format!("[0:v:0][0:v:1]alphamerge,format={pix_fmt}"));
	} else {
		cmd.args(["-pix_fmt", pix_fmt]);
	}
	cmd.args(["-frames:v", "1", "-c:v", "pam", "-f", "image2pipe", "-"]);
	cmd
}

pub async fn run_pam(cmd: &ToolCommand, cancel: &CancellationToken) -> Result<(DynamicImage, usize), JobError> {
	let out = process::run(cmd, StdoutMode::Capture, |_| {}, cancel).await?;
	if !out.status.success() || out.stdout.is_empty() {
		let reason = process::failure_reason(&out.stderr_tail);
		return Err(JobError::with_log(
			ErrorKind::SourceDamaged(format!("ffmpeg could not decode the image: {reason}")),
			out.stderr_tail,
		));
	}

	let counted = out.counted_lines;
	let image = blocking(cancel, move || {
		image::load_from_memory_with_format(&out.stdout, image::ImageFormat::Pnm)
	})
	.await?
	.map_err(|err| ErrorKind::SourceDamaged(format!("ffmpeg's decoded pixels are unreadable: {err}")))?;
	Ok((image, counted))
}

async fn count_decode_errors(tools: &Toolchain, source: &Path, cancel: &CancellationToken) -> Result<usize, JobError> {
	let mut cmd = ToolCommand::new(&tools.ffmpeg, STALL_TIMEOUT);
	cmd.count_lines_containing(IMAGE_DECODE_ERRORS);
	cmd.args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
		.arg(process::tool_path(source))
		.args(["-f", "null", "-"]);
	Ok(process::run(&cmd, StdoutMode::Capture, |_| {}, cancel)
		.await?
		.counted_lines)
}

pub fn oriented(mut image: DynamicImage, orientation: u16) -> DynamicImage {
	if let Some(orientation) = u8::try_from(orientation).ok().and_then(Orientation::from_exif) {
		image.apply_orientation(orientation);
	}
	image
}

fn decode_jxl(bytes: &[u8]) -> Result<Decoded, Undecodable> {
	let image = JxlImage::builder().read(bytes).map_err(damaged)?;
	if image.image_header().metadata.animation.is_some() {
		return Err(keep("animated JPEG XL files are already efficient"));
	}
	let (icc, cicp) = jxl_colour(&image)?;

	let render = image.render_frame(0).map_err(damaged)?;
	let mut stream = render.stream();
	let (width, height, channels) = (stream.width(), stream.height(), stream.channels());
	let len = usize::try_from(u64::from(width) * u64::from(height) * u64::from(channels)).map_err(damaged)?;
	let mut samples = vec![0u16; len];
	stream.write_to_buffer(&mut samples);

	let decoded = match channels {
		1 => ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageLuma16),
		2 => ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageLumaA16),
		3 => ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageRgb16),
		_ => ImageBuffer::from_raw(width, height, samples).map(DynamicImage::ImageRgba16),
	};
	Ok(Decoded {
		image: decoded.ok_or_else(|| damaged("the rendered frame has the wrong size"))?,
		icc,
		cicp,
		decode_errors: 0,
	})
}

fn jxl_colour(image: &JxlImage) -> Result<(Option<Vec<u8>>, Option<Cicp>), Undecodable> {
	let xyb = image.image_header().metadata.xyb_encoded;
	if let Some(icc) = image.original_icc() {
		return if xyb {
			Err(keep("this JPEG XL colour profile can't be carried over"))
		} else {
			Ok((Some(icc.to_vec()), None))
		};
	}

	match image.rendered_cicp() {
		Some([primaries @ (1 | 9 | 12), transfer @ (1 | 13), ..]) => Ok((None, Some(Cicp { primaries, transfer }))),
		_ => Err(keep("this JPEG XL colour space can't be carried over")),
	}
}
