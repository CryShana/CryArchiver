use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;

use image::DynamicImage;
use jxl_oxide::JxlImage;
use tokio_util::sync::CancellationToken;

use crate::container::avif::{self, AvifExpectations, AvifMetadata};
use crate::container::jxl;
use crate::images::blocking;
use crate::images::decode::{PamRequest, pam_command, run_pam};
use crate::images::ssim;
use crate::metadata::OutputMetadata;
use crate::metadata::icc::Cicp;
use crate::outcome::{ErrorKind, JobError};
use crate::toolchain::Toolchain;

#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
	pub width: u32,
	pub height: u32,
	pub alpha: bool,
	pub icc: Option<&'a [u8]>,
	pub cicp: Cicp,
	pub metadata: &'a OutputMetadata,
}

pub async fn verify_avif(
	tools: &Toolchain,
	output: &Path,
	bytes: &[u8],
	reference: Arc<DynamicImage>,
	expected: &Expected<'_>,
	cancel: &CancellationToken,
) -> Result<f64, JobError> {
	let expectations = AvifExpectations {
		width: expected.width,
		height: expected.height,
		alpha: expected.alpha,
		cicp: expected.cicp,
		metadata: AvifMetadata {
			icc: expected.icc,
			exif: expected.metadata.exif.as_deref(),
			xmp: expected.metadata.xmp.as_deref(),
		},
	};
	avif::check(bytes, &expectations).map_err(failed)?;

	let request = PamRequest {
		alpha_item: expected.alpha,
		deep: true,
		keep_stored_orientation: false,
	};
	let (decoded, errors) = run_pam(&pam_command(&tools.ffmpeg, output, request), cancel)
		.await
		.map_err(|err| failed(err.kind))?;
	if errors > 0 {
		return Err(failed(format!("the output decodes with {errors} errors")));
	}

	blocking(cancel, move || ssim::score(&reference, &decoded))
		.await?
		.map_err(failed)
}

pub async fn verify_jxl(
	bytes: Arc<Vec<u8>>,
	reference: Arc<DynamicImage>,
	expected: &Expected<'_>,
	lossless: bool,
	cancel: &CancellationToken,
) -> Result<f64, JobError> {
	check_jxl(&bytes, expected, lossless).map_err(failed)?;

	let source_icc = expected.icc.map(<[u8]>::to_vec);
	blocking(cancel, move || {
		let reference = ssim::to_srgb(&reference, source_icc.as_deref())?;
		let rendered = ssim::render_jxl_srgb(&bytes)?;
		ssim::score(&reference, &rendered)
	})
	.await?
	.map_err(failed)
}

pub fn check_jxl(bytes: &[u8], expected: &Expected<'_>, lossless: bool) -> Result<(), String> {
	let boxes = jxl::read_boxes(bytes).map_err(|err| err.to_string())?;
	if !boxes.container {
		return Err("the output is not a JPEG XL container".into());
	}
	if boxes.exif.as_deref() != expected.metadata.exif.as_deref() {
		return Err("the Exif box is missing or changed".into());
	}
	if boxes.xmp.as_deref() != expected.metadata.xmp.as_deref() {
		return Err("the XMP box is missing or changed".into());
	}

	let image = JxlImage::builder()
		.read(bytes)
		.map_err(|err| format!("jxl-oxide could not read the output: {err}"))?;
	if (image.width(), image.height()) != (expected.width, expected.height) {
		return Err(format!(
			"the output is {}×{} but {}×{} was expected",
			image.width(),
			image.height(),
			expected.width,
			expected.height
		));
	}
	if image.image_header().metadata.orientation != 1 {
		return Err("the output is not stored upright".into());
	}
	if image.pixel_format().has_alpha() != expected.alpha {
		return Err("the alpha channel does not match the source".into());
	}
	if lossless && expected.icc.is_some() && image.original_icc() != expected.icc {
		return Err("the ICC profile was not carried over unchanged".into());
	}
	Ok(())
}

fn failed(reason: impl Display) -> JobError {
	ErrorKind::EncodeFailed(format!("verification failed: {reason}")).into()
}
