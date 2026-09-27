use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::{DynamicImage, GenericImageView};
use tokio_util::sync::CancellationToken;

use crate::engine::{JobEvent, Stage};
use crate::images::animated::{self, Animation};
use crate::images::avif::{self, AvifColour};
use crate::images::decode::{self, Decoded, Undecodable};
use crate::images::jxl::{self, JxlJob, JxlPixels};
use crate::images::plan::{self, Candidate, Gate, PrecheckInput, StillFacts, StillPlan};
use crate::images::props::{self, Effective};
use crate::images::verify::{self, Expected};
use crate::images::{blocking, structure};
use crate::metadata::icc::{self, Cicp};
use crate::metadata::{self, ImageFormat, MetadataMode, OutputMetadata, SourceMetadata, exif, sniff};
use crate::outcome::{ErrorKind, IoSide, JobError, KeepReason};
use crate::preset::Speed;
use crate::probe::MediaInfo;
use crate::toolchain::Toolchain;
use crate::video::run_ffmpeg;
use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub struct ImageInput {
	pub bytes: Arc<Vec<u8>>,
	pub metadata: SourceMetadata,
	pub warnings: Vec<String>,
	pub pix_fmt: Option<String>,
	pub animated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCaps {
	pub libjxl: bool,
	pub libaom: bool,
}

pub struct ImageContext<'a> {
	pub tools: &'a Toolchain,
	pub source: &'a Path,
	pub source_bytes: u64,
	pub speed: Speed,
	pub min_savings: f64,
	pub mode: MetadataMode,
	pub threads: usize,
	pub caps: ImageCaps,
	pub on_event: &'a (dyn Fn(JobEvent) + Sync),
	pub cancel: &'a CancellationToken,
}

#[derive(Debug)]
pub enum ImageOutcome {
	Output {
		path: PathBuf,
		bytes: u64,
		warnings: Vec<String>,
	},
	Kept(KeepReason),
}

struct Still {
	image: Arc<DynamicImage>,
	icc: Option<Vec<u8>>,
	cicp: Option<Cicp>,
	effective: Effective,
	plan: StillPlan,
	metadata: OutputMetadata,
	warnings: Vec<String>,
}

#[derive(Default)]
struct Choice {
	best: Option<(PathBuf, u64)>,
	best_rejected: Option<f64>,
	smallest: Option<u64>,
}

pub fn prepare(
	source: &Path,
	info: &MediaInfo,
	animated: bool,
	drop_aux: bool,
	mode: MetadataMode,
) -> Result<Result<ImageInput, KeepReason>, JobError> {
	let bytes = fs::read(source).map_err(|err| JobError::io(IoSide::Source(source), &err))?;
	let structure = structure::check(&bytes);
	let metadata = metadata::extract(&bytes);
	let video = info.streams.iter().find(|stream| stream.is_video());
	let probed = video.and_then(|stream| Some((stream.width?, stream.height?)));

	let input = PrecheckInput {
		structure: &structure,
		metadata: &metadata,
		tiff_pages: (sniff(&bytes) == ImageFormat::Tiff)
			.then(|| exif::tiff_pages(&bytes))
			.flatten(),
		dimensions: decode::header_dimensions(&bytes, probed),
		drop_aux,
		mode,
	};
	match plan::precheck(&input) {
		Err(Gate::Damaged(reason)) => return Err(ErrorKind::SourceDamaged(reason).into()),
		Err(Gate::Keep(reason)) => return Ok(Err(reason)),
		Ok(()) => {}
	}

	let warnings = structure.warnings.iter().chain(&metadata.warnings).cloned().collect();
	Ok(Ok(ImageInput {
		bytes: Arc::new(bytes),
		metadata,
		warnings,
		pix_fmt: video.and_then(|stream| stream.pix_fmt.clone()),
		animated,
	}))
}

pub async fn produce(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	workspace: &Workspace,
) -> Result<ImageOutcome, JobError> {
	if input.animated
		&& let Some(outcome) = Box::pin(produce_animation(cx, input, workspace)).await?
	{
		return Ok(outcome);
	}

	let still = match Box::pin(analyse(cx, input)).await? {
		Ok(still) => still,
		Err(reason) => return Ok(ImageOutcome::Kept(reason)),
	};
	Box::pin(encode_and_choose(cx, input, workspace, still)).await
}

async fn analyse(cx: &ImageContext<'_>, input: &ImageInput) -> Result<Result<Still, KeepReason>, JobError> {
	(cx.on_event)(JobEvent::Stage(Stage::Analyze));
	let Decoded {
		image,
		icc,
		cicp,
		decode_errors,
	} = match decode::decode(
		cx.tools,
		cx.source,
		Arc::clone(&input.bytes),
		&input.metadata,
		input.pix_fmt.as_deref(),
		cx.cancel,
	)
	.await
	{
		Ok(decoded) => decoded,
		Err(Undecodable::Keep(reason)) => return Ok(Err(reason)),
		Err(Undecodable::Failed(err)) => return Err(err),
	};

	let lossy = props::lossy_source(&input.bytes);
	let image = Arc::new(image);
	let (effective, graphic) = {
		let image = Arc::clone(&image);
		blocking(cx.cancel, move || {
			let effective = props::effective(&image);
			let graphic = !lossy && props::is_graphic(&props::graphic_metrics(&props::rgb8(&image)), lossy);
			(effective, graphic)
		})
		.await?
	};

	let (width, height) = image.dimensions();
	let facts = StillFacts {
		width,
		height,
		effective,
		graphic,
		grey_icc: grey_profile(icc.as_deref()),
	};
	let plan = match plan::plan_still(&facts, cx.speed, cx.caps.libjxl) {
		Ok(plan) => plan,
		Err(reason) => return Ok(Err(reason)),
	};

	let metadata = metadata::build_output(&input.metadata, cx.mode, width, height);
	let mut warnings = input.warnings.clone();
	warnings.extend(metadata.warnings.iter().cloned());
	if decode_errors > 0 {
		warnings.push(format!("{decode_errors} decoding errors were concealed"));
	}
	Ok(Ok(Still {
		image,
		icc,
		cicp,
		effective,
		plan,
		metadata,
		warnings,
	}))
}

async fn encode_and_choose(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	workspace: &Workspace,
	still: Still,
) -> Result<ImageOutcome, JobError> {
	let (width, height) = still.image.dimensions();
	let colour = AvifColour {
		icc: still.icc.as_deref(),
		cicp: still.cicp,
	};
	let expected = Expected {
		width,
		height,
		alpha: still.effective.alpha,
		icc: still.icc.as_deref(),
		cicp: avif::output_cicp(colour),
		metadata: &still.metadata,
	};
	let reference = eight_bit_reference(cx, &still).await?;
	let mut choice = Choice::default();

	for (index, candidate) in still.plan.candidates.iter().enumerate() {
		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let path = workspace.file(&format!("candidate{index}.{}", candidate.extension()));
		let bytes = encode_candidate(cx, input, workspace, &still, *candidate, colour, reference.as_ref()).await?;
		fs::write(&path, &bytes).map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;

		let size = bytes.len() as u64;
		choice.smallest = Some(choice.smallest.map_or(size, |smallest| smallest.min(size)));
		let beaten = choice.best.as_ref().is_some_and(|(_, best)| *best <= size);
		if !saves_enough(cx.source_bytes, size, cx.min_savings) || beaten {
			continue;
		}

		(cx.on_event)(JobEvent::Stage(Stage::Verify));
		let score = match candidate {
			Candidate::Ravif(_) => {
				let reference = Arc::clone(reference.as_ref().expect("prepared for AVIF candidates"));
				verify::verify_avif(cx.tools, &path, &bytes, reference, &expected, cx.cancel).await?
			}
			Candidate::JxlLossy(_) | Candidate::JxlLossless(_) => {
				let job = jxl_job(cx, input, &still, *candidate);
				let source_reference = jxl::reference(&cx.tools.ffmpeg, &job, cx.cancel).await?;
				let lossless = matches!(candidate, Candidate::JxlLossless(_));
				verify::verify_jxl(Arc::new(bytes), source_reference, &expected, lossless, cx.cancel).await?
			}
		};
		if score >= still.plan.target {
			choice.best = Some((path, size));
		} else {
			choice.best_rejected = Some(choice.best_rejected.map_or(score, |best| best.max(score)));
		}
	}

	finish_choice(cx, workspace, choice, still.plan.target, still.warnings)
}

async fn eight_bit_reference(cx: &ImageContext<'_>, still: &Still) -> Result<Option<Arc<DynamicImage>>, JobError> {
	if !still
		.plan
		.candidates
		.iter()
		.any(|candidate| matches!(candidate, Candidate::Ravif(_)))
	{
		return Ok(None);
	}

	let image = Arc::clone(&still.image);
	let alpha = still.effective.alpha;
	let reference = blocking(cx.cancel, move || {
		if alpha {
			DynamicImage::ImageRgba8(props::rgba8(&image))
		} else {
			DynamicImage::ImageRgb8(props::rgb8(&image))
		}
	})
	.await?;
	Ok(Some(Arc::new(reference)))
}

async fn encode_candidate(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	workspace: &Workspace,
	still: &Still,
	candidate: Candidate,
	colour: AvifColour<'_>,
	reference: Option<&Arc<DynamicImage>>,
) -> Result<Vec<u8>, JobError> {
	match candidate {
		Candidate::Ravif(settings) => {
			let pixels = Arc::clone(reference.expect("prepared for AVIF candidates"));
			let (alpha, threads) = (still.effective.alpha, cx.threads);
			let encoded = blocking(cx.cancel, move || avif::encode(&pixels, alpha, settings, threads))
				.await?
				.map_err(ErrorKind::EncodeFailed)?;
			Ok(avif::finish(&encoded, colour, &still.metadata).map_err(ErrorKind::EncodeFailed)?)
		}
		Candidate::JxlLossy(_) | Candidate::JxlLossless(_) => {
			let job = jxl_job(cx, input, still, candidate);
			jxl::encode(
				&cx.tools.ffmpeg,
				&job,
				&workspace.file("libjxl.jxl"),
				&still.metadata,
				cx.cancel,
			)
			.await
		}
	}
}

fn jxl_job<'a>(cx: &ImageContext<'a>, input: &ImageInput, still: &Still, candidate: Candidate) -> JxlJob<'a> {
	let settings = match candidate {
		Candidate::JxlLossy(settings) | Candidate::JxlLossless(settings) => settings,
		Candidate::Ravif(_) => unreachable!("only JPEG XL candidates run through libjxl"),
	};
	JxlJob {
		source: cx.source,
		source_is_heif: sniff(&input.bytes) == ImageFormat::Heif,
		settings,
		pixels: JxlPixels::for_source(still.effective, grey_profile(still.icc.as_deref())),
	}
}

fn finish_choice(
	cx: &ImageContext<'_>,
	workspace: &Workspace,
	choice: Choice,
	target: f64,
	warnings: Vec<String>,
) -> Result<ImageOutcome, JobError> {
	if let Some((path, bytes)) = choice.best {
		let extension = path.extension().unwrap_or_default().to_string_lossy().into_owned();
		let output = workspace.file(&format!("out.{extension}"));
		fs::rename(&path, &output).map_err(|err| JobError::io(IoSide::Destination(&output), &err))?;
		for warning in &warnings {
			(cx.on_event)(JobEvent::Warning(warning.clone()));
		}
		return Ok(ImageOutcome::Output {
			path: output,
			bytes,
			warnings,
		});
	}

	Ok(ImageOutcome::Kept(match choice.best_rejected {
		Some(score) => KeepReason::QualityUnreachable(format!(
			"quality target not reached at a smaller size (SSIMULACRA2 {score:.1}, target {target:.0})"
		)),
		None => KeepReason::NotSmaller {
			src_bytes: cx.source_bytes,
			out_bytes: choice.smallest.unwrap_or(cx.source_bytes),
		},
	}))
}

async fn produce_animation(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	workspace: &Workspace,
) -> Result<Option<ImageOutcome>, JobError> {
	(cx.on_event)(JobEvent::Stage(Stage::Analyze));
	let bytes = Arc::clone(&input.bytes);
	let timeline = match blocking(cx.cancel, move || animated::scan(&bytes)).await? {
		Ok(Some(timeline)) if timeline.frames > 1 => timeline,
		Ok(_) => return Ok(None),
		Err(reason) => return Err(ErrorKind::SourceDamaged(reason).into()),
	};
	let animation = Animation {
		alpha: animated::has_alpha(cx.tools, cx.source, cx.cancel).await?,
		timeline,
	};
	let settings = match animated::plan_animation(cx.speed, cx.caps.libaom) {
		Ok(settings) => settings,
		Err(reason) => return Ok(Some(ImageOutcome::Kept(reason))),
	};

	let mut warnings = input.warnings.clone();
	let personal = input.metadata.exif.is_some() || input.metadata.xmp.is_some() || !input.metadata.comments.is_empty();
	if cx.mode == MetadataMode::Full && personal {
		warnings.push("metadata is not carried into animated AVIF yet".into());
	}

	(cx.on_event)(JobEvent::Stage(Stage::Encode));
	let output = workspace.file("out.avif");
	let cmd = animated::encode_command(&cx.tools.ffmpeg, cx.source, &output, &animation, settings);
	let frames = f64::from(animation.timeline.frames);
	run_ffmpeg(
		&cmd,
		|progress| {
			if let Some(frame) = progress.frame {
				#[allow(clippy::cast_precision_loss)]
				(cx.on_event)(JobEvent::Progress((frame as f64 / frames).clamp(0.0, 1.0)));
			}
		},
		cx.cancel,
	)
	.await?;

	let bytes = fs::metadata(&output)
		.map_err(|err| JobError::io(IoSide::Destination(&output), &err))?
		.len();
	if !saves_enough(cx.source_bytes, bytes, cx.min_savings) {
		return Ok(Some(ImageOutcome::Kept(KeepReason::NotSmaller {
			src_bytes: cx.source_bytes,
			out_bytes: bytes,
		})));
	}

	(cx.on_event)(JobEvent::Stage(Stage::Verify));
	let score = animated::verify_animation(cx.tools, cx.source, &output, &animation, cx.cancel).await?;
	let target = plan::ssim_target(cx.speed);
	if score < target {
		return Ok(Some(ImageOutcome::Kept(KeepReason::QualityUnreachable(format!(
			"quality target not reached at a smaller size (SSIMULACRA2 {score:.1}, target {target:.0})"
		)))));
	}

	for warning in &warnings {
		(cx.on_event)(JobEvent::Warning(warning.clone()));
	}
	Ok(Some(ImageOutcome::Output {
		path: output,
		bytes,
		warnings,
	}))
}

fn grey_profile(icc: Option<&[u8]>) -> bool {
	icc.and_then(icc::colour_space) == Some(*b"GRAY")
}

#[allow(clippy::cast_precision_loss)]
fn saves_enough(src_bytes: u64, out_bytes: u64, min_savings: f64) -> bool {
	(out_bytes as f64) <= (src_bytes as f64) * (1.0 - min_savings)
}
