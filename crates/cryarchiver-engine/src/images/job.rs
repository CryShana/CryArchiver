use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::{DynamicImage, GenericImageView};
use tokio_util::sync::CancellationToken;

use crate::engine::{JobEvent, Stage};
use crate::images::animated::{self, Animation, ScanError};
use crate::images::avif::{self, AvifColour};
use crate::images::decode::{self, Decoded, Undecodable};
use crate::images::jxl::{self, JxlJob, JxlPixels};
use crate::images::plan::{self, Candidate, Gate, ImageRoute, PrecheckInput, StillFacts, StillPlan};
use crate::images::props::{self, Effective};
use crate::images::verify::{self, Expected};
use crate::images::{blocking, structure};
use crate::metadata::icc::{self, Cicp};
use crate::metadata::{self, ImageFormat, MetadataMode, OutputMetadata, SourceMetadata, exif, sniff};
use crate::options::kept_as_requested;
use crate::outcome::{ErrorKind, IoSide, JobError, KeepReason, Refusal};
use crate::preset::{Overrides, Speed};
use crate::probe::MediaInfo;
use crate::quality::keys::{CacheKey, Group, ImageGroup};
use crate::quality::memory::{Observation, QualityMemory};
use crate::toolchain::Toolchain;
use crate::video::run_ffmpeg;
use crate::workspace::Workspace;

const ANIMATED_JXL: &str = "animated JPEG XL output isn't supported; use -f avif";

#[derive(Debug, Clone)]
pub struct ImageInput {
	pub bytes: Arc<Vec<u8>>,
	pub metadata: SourceMetadata,
	pub warnings: Vec<String>,
	pub pix_fmt: Option<String>,
	pub animated: bool,
	pub memory_bytes: u64,
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
	pub overrides: Overrides,
	pub min_savings: f64,
	pub mode: MetadataMode,
	pub threads: usize,
	pub caps: ImageCaps,
	pub on_event: &'a (dyn Fn(JobEvent) + Sync),
	pub cancel: &'a CancellationToken,
	pub memory: &'a dyn QualityMemory,
	pub route: Option<ImageRoute>,
	pub forced: bool,
}

#[derive(Debug)]
pub enum ImageOutcome {
	Output {
		path: PathBuf,
		bytes: u64,
		warnings: Vec<String>,
		/// Settings the groups learn once the output is committed.
		learned: Vec<(CacheKey, Observation)>,
	},
	Kept(KeepReason),
}

struct Still {
	image: Arc<DynamicImage>,
	icc: Option<Vec<u8>>,
	cicp: Option<Cicp>,
	effective: Effective,
	graphic: bool,
	plan: StillPlan,
	metadata: OutputMetadata,
	warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct Choice {
	best: Option<(PathBuf, u64)>,
	/// The score of `best` when it misses the target and is written only because it was requested.
	best_short: Option<f64>,
	best_rejected: Option<f64>,
	smallest: Option<u64>,
	failures: Vec<(Candidate, JobError)>,
}

#[derive(Debug)]
enum Verdict {
	Output(PathBuf, u64),
	Kept(KeepReason),
	Failed(JobError),
}

struct Env<'a> {
	colour: AvifColour<'a>,
	expected: Expected<'a>,
	reference: Option<Arc<DynamicImage>>,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
	index: usize,
	best: Option<u64>,
	table: Option<Candidate>,
}

#[derive(Debug)]
enum Tried {
	Passed {
		path: PathBuf,
		size: u64,
		candidate: Candidate,
		score: f64,
		short: bool,
	},
	Rejected {
		size: u64,
		score: f64,
	},
	TooLarge {
		size: u64,
		score: Option<f64>,
	},
	Failed(JobError),
}

impl Choice {
	fn note(&mut self, size: u64) {
		self.smallest = Some(self.smallest.map_or(size, |smallest| smallest.min(size)));
	}

	fn reject(&mut self, score: f64) {
		self.best_rejected = Some(self.best_rejected.map_or(score, |best| best.max(score)));
	}

	fn passing_size(&self) -> Option<u64> {
		self.best
			.as_ref()
			.filter(|_| self.best_short.is_none())
			.map(|(_, size)| *size)
	}

	fn offer(&mut self, path: PathBuf, size: u64, short: Option<f64>) {
		let better = match (short, self.best_short) {
			(None, None) => self.best.as_ref().is_none_or(|(_, best)| size < *best),
			(None, Some(_)) => true,
			(Some(_), None) => self.best.is_none(),
			(Some(score), Some(best)) => score > best,
		};

		if better {
			self.best = Some((path, size));
			self.best_short = short;
		}
	}

	fn short_note(&self, target: f64) -> Option<String> {
		self.best_short.map(|score| short_warning(score, target))
	}
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
	let dimensions = decode::header_dimensions(&bytes, probed);

	let input = PrecheckInput {
		structure: &structure,
		metadata: &metadata,
		tiff_pages: (sniff(&bytes) == ImageFormat::Tiff)
			.then(|| exif::tiff_pages(&bytes))
			.flatten(),
		dimensions,
		drop_aux,
		mode,
	};
	match plan::precheck(&input) {
		Err(Gate::Damaged(reason)) => return Err(ErrorKind::SourceDamaged(reason).into()),
		Err(Gate::Keep(reason)) => return Ok(Err(reason)),
		Ok(()) => {}
	}

	let warnings = structure.warnings.iter().chain(&metadata.warnings).cloned().collect();
	let sequence = info
		.streams
		.iter()
		.any(|stream| stream.is_video() && stream.nb_frames.is_some_and(|frames| frames > 1));
	Ok(Ok(ImageInput {
		bytes: Arc::new(bytes),
		metadata,
		warnings,
		pix_fmt: video.and_then(|stream| stream.pix_fmt.clone()),
		animated: animated || sequence,
		memory_bytes: dimensions.map_or(0, |(width, height)| plan::worst_case_memory(width, height)),
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
	let plan = match plan::plan_still(&facts, cx.speed, &cx.overrides, cx.caps.libjxl, cx.route) {
		Ok(plan) => plan,
		Err(Refusal::Keep(reason)) => return Ok(Err(reason)),
		Err(Refusal::Impossible(reason)) => return Err(ErrorKind::EncodeFailed(reason).into()),
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
		graphic,
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
	let env = Env {
		colour,
		expected: Expected {
			width,
			height,
			alpha: still.effective.alpha,
			icc: still.icc.as_deref(),
			cicp: avif::output_cicp(colour),
			metadata: &still.metadata,
		},
		reference: eight_bit_reference(cx, &still).await?,
	};
	let mut choice = Choice::default();
	let mut learned = Vec::new();

	for (index, planned) in still.plan.candidates.iter().copied().enumerate() {
		let key = candidate_key(cx.tools, input, &still, planned);
		let candidate = key
			.as_ref()
			.and_then(|key| cx.memory.lookup(key))
			.map_or(planned, |stats| planned.with_setting(cx.speed, stats.setting));
		let slot = Slot {
			index,
			best: choice.passing_size(),
			table: candidate.finer_than(planned).then_some(planned),
		};

		match Box::pin(try_candidate(cx, input, workspace, &still, &env, candidate, slot)).await? {
			Tried::Passed {
				path,
				size,
				candidate,
				score,
				short,
			} => {
				choice.note(size);
				choice.offer(path, size, short.then_some(score));
				if !short
					&& let Some(key) = key
					&& let Some(setting) = candidate.needed_setting(score, still.plan.target, cx.speed)
				{
					learned.push((
						key,
						Observation {
							setting,
							slope: None,
							bpp: None,
						},
					));
				}
			}
			Tried::Rejected { size, score } => {
				choice.note(size);
				choice.reject(score);
			}
			Tried::TooLarge { size, score } => {
				choice.note(size);
				if let Some(score) = score {
					choice.reject(score);
				}
			}
			Tried::Failed(err) => choice.failures.push((candidate, err)),
		}
	}

	let mut notes: Vec<String> = choice.short_note(still.plan.target).into_iter().collect();
	if cx.forced
		&& let Some((_, size)) = &choice.best
		&& !saves_enough(cx.source_bytes, *size, cx.min_savings)
	{
		notes.push(kept_as_requested(cx.source_bytes, *size));
	}

	let mut warnings = still.warnings;
	warnings.extend(notes);
	finish_choice(cx, workspace, choice, still.plan.target, warnings, learned)
}

async fn try_candidate(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	workspace: &Workspace,
	still: &Still,
	env: &Env<'_>,
	mut candidate: Candidate,
	slot: Slot,
) -> Result<Tried, JobError> {
	let mut retries = 0;
	let mut last_score = None;
	let mut table = slot.table;

	loop {
		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let path = workspace.file(&format!("candidate{}.{}", slot.index, candidate.extension()));
		let bytes = encode_candidate(
			cx,
			input,
			workspace,
			still,
			candidate,
			env.colour,
			env.reference.as_ref(),
		)
		.await?;
		fs::write(&path, &bytes).map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;

		let size = bytes.len() as u64;
		let too_large = !cx.forced && !saves_enough(cx.source_bytes, size, cx.min_savings);
		if too_large
			&& last_score.is_none()
			&& let Some(fallback) = table.take()
		{
			candidate = fallback;
			continue;
		}

		if too_large || slot.best.is_some_and(|best| best <= size) {
			return Ok(Tried::TooLarge {
				size,
				score: last_score,
			});
		}

		(cx.on_event)(JobEvent::Stage(Stage::Verify));
		let score = match verify_candidate(cx, still, env, candidate, &path, bytes).await {
			Ok(score) => score,
			Err(err) if matches!(err.kind, ErrorKind::EncodeFailed(_)) => return Ok(Tried::Failed(err)),
			Err(err) => return Err(err),
		};
		if score >= still.plan.target {
			return Ok(Tried::Passed {
				path,
				size,
				candidate,
				score,
				short: false,
			});
		}

		match candidate
			.raised(score, still.plan.target)
			.filter(|_| retries < plan::MAX_RETRIES)
		{
			Some(raised) => {
				candidate = raised;
				retries += 1;
				last_score = Some(score);
			}
			None if cx.forced => {
				return Ok(Tried::Passed {
					path,
					size,
					candidate,
					score,
					short: true,
				});
			}
			None => return Ok(Tried::Rejected { size, score }),
		}
	}
}

async fn verify_candidate(
	cx: &ImageContext<'_>,
	still: &Still,
	env: &Env<'_>,
	candidate: Candidate,
	path: &Path,
	bytes: Vec<u8>,
) -> Result<f64, JobError> {
	match candidate {
		Candidate::Ravif(_) => {
			let reference = Arc::clone(env.reference.as_ref().expect("prepared for AVIF candidates"));
			verify::verify_avif(cx.tools, path, &bytes, reference, &env.expected, cx.cancel).await
		}
		Candidate::JxlLossy(_) | Candidate::JxlLossless(_) => {
			let lossless = matches!(candidate, Candidate::JxlLossless(_));
			verify::verify_jxl(
				Arc::new(bytes),
				Arc::clone(&still.image),
				&env.expected,
				lossless,
				cx.cancel,
			)
			.await
		}
	}
}

fn candidate_key(tools: &Toolchain, input: &ImageInput, still: &Still, candidate: Candidate) -> Option<CacheKey> {
	candidate.setting()?;
	let (width, height) = still.image.dimensions();

	Some(CacheKey {
		group: Group::Image(ImageGroup::new(
			sniff(&input.bytes),
			still.graphic,
			still.effective.alpha,
			still.effective.high_depth,
			width,
			height,
		)),
		encoder: candidate.encoder_id(tools),
		preset: candidate.preset(),
		target: still.plan.target,
		floor: None,
	})
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
		layout: decode::layout(&input.bytes, &input.metadata),
		settings,
		pixels: JxlPixels::for_source(still.effective, grey_profile(still.icc.as_deref())),
	}
}

fn finish_choice(
	cx: &ImageContext<'_>,
	workspace: &Workspace,
	choice: Choice,
	target: f64,
	mut warnings: Vec<String>,
	learned: Vec<(CacheKey, Observation)>,
) -> Result<ImageOutcome, JobError> {
	for (candidate, err) in &choice.failures {
		let reason = match &err.kind {
			ErrorKind::EncodeFailed(reason) => reason.clone(),
			other => other.to_string(),
		};
		warnings.push(format!(
			"the {} candidate was discarded ({reason})",
			candidate_name(*candidate)
		));
	}

	match verdict(choice, target, cx.source_bytes) {
		Verdict::Output(path, bytes) => {
			let extension = path.extension().unwrap_or_default().to_string_lossy().into_owned();
			let output = workspace.file(&format!("out.{extension}"));
			fs::rename(&path, &output).map_err(|err| JobError::io(IoSide::Destination(&output), &err))?;
			for warning in &warnings {
				(cx.on_event)(JobEvent::Warning(warning.clone()));
			}
			Ok(ImageOutcome::Output {
				path: output,
				bytes,
				warnings,
				learned,
			})
		}
		Verdict::Kept(reason) => Ok(ImageOutcome::Kept(reason)),
		Verdict::Failed(err) => Err(err),
	}
}

fn verdict(choice: Choice, target: f64, source_bytes: u64) -> Verdict {
	if let Some((path, bytes)) = choice.best {
		return Verdict::Output(path, bytes);
	}
	if let Some(score) = choice.best_rejected {
		return Verdict::Kept(KeepReason::QualityUnreachable(format!(
			"quality target not reached at a smaller size (SSIMULACRA2 {score:.1}, target {target:.0})"
		)));
	}
	if let Some((_, err)) = choice.failures.into_iter().next() {
		return Verdict::Failed(err);
	}
	Verdict::Kept(KeepReason::NotSmaller {
		src_bytes: source_bytes,
		out_bytes: choice.smallest.unwrap_or(source_bytes),
	})
}

fn candidate_name(candidate: Candidate) -> &'static str {
	match candidate {
		Candidate::Ravif(_) => "AVIF",
		Candidate::JxlLossy(_) | Candidate::JxlLossless(_) => "JPEG XL",
	}
}

async fn produce_animation(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	workspace: &Workspace,
) -> Result<Option<ImageOutcome>, JobError> {
	(cx.on_event)(JobEvent::Stage(Stage::Analyze));
	if matches!(
		sniff(&input.bytes),
		ImageFormat::Avif | ImageFormat::Heif | ImageFormat::Jxl
	) {
		if let Some(route) = cx.route {
			return Err(animation_refusal(
				route,
				"animated AVIF/HEIF/JPEG XL files can't be re-encoded",
			));
		}

		return Ok(Some(keep("animated AVIF/HEIF/JPEG XL files are kept as they are")));
	}

	let bytes = Arc::clone(&input.bytes);
	let timeline = match blocking(cx.cancel, move || animated::scan(&bytes)).await? {
		Ok(Some(timeline)) if timeline.frames > 1 => timeline,
		Ok(_) => return Ok(None),
		Err(ScanError::TooManyFrames) => {
			let reason = "the animation has too many frames to verify";
			if let Some(route) = cx.route {
				return Err(animation_refusal(route, reason));
			}

			return Ok(Some(keep(reason)));
		}
		Err(ScanError::Unreadable(reason)) => return Err(ErrorKind::SourceDamaged(reason).into()),
	};

	if matches!(cx.route, Some(ImageRoute::Jxl | ImageRoute::JxlLossless)) {
		return Err(ErrorKind::EncodeFailed(ANIMATED_JXL.into()).into());
	}

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
	if input.metadata.icc.is_some() {
		warnings.push("the colour profile is not carried into animated AVIF yet".into());
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
		if !cx.forced {
			return Ok(Some(ImageOutcome::Kept(KeepReason::NotSmaller {
				src_bytes: cx.source_bytes,
				out_bytes: bytes,
			})));
		}

		warnings.push(kept_as_requested(cx.source_bytes, bytes));
	}

	(cx.on_event)(JobEvent::Stage(Stage::Verify));
	let score = animated::verify_animation(cx.tools, cx.source, &output, &animation, cx.cancel).await?;
	let target = plan::image_target(cx.speed, &cx.overrides);
	if score < target {
		if !cx.forced {
			return Ok(Some(ImageOutcome::Kept(KeepReason::QualityUnreachable(format!(
				"quality target not reached at a smaller size (SSIMULACRA2 {score:.1}, target {target:.0})"
			)))));
		}

		warnings.push(short_warning(score, target));
	}

	for warning in &warnings {
		(cx.on_event)(JobEvent::Warning(warning.clone()));
	}
	Ok(Some(ImageOutcome::Output {
		path: output,
		bytes,
		warnings,
		learned: Vec::new(),
	}))
}

fn animation_refusal(route: ImageRoute, avif_reason: &str) -> JobError {
	let reason = match route {
		ImageRoute::Jxl | ImageRoute::JxlLossless => ANIMATED_JXL,
		ImageRoute::Avif => avif_reason,
	};
	ErrorKind::EncodeFailed(reason.to_owned()).into()
}

fn short_warning(score: f64, target: f64) -> String {
	format!(
		"quality target not reached (SSIMULACRA2 {score:.1}, target {target:.0}); it was written because it was requested"
	)
}

fn keep(reason: &str) -> ImageOutcome {
	ImageOutcome::Kept(KeepReason::Unsupported(reason.to_owned()))
}

fn grey_profile(icc: Option<&[u8]>) -> bool {
	icc.and_then(icc::colour_space) == Some(*b"GRAY")
}

#[allow(clippy::cast_precision_loss)]
fn saves_enough(src_bytes: u64, out_bytes: u64, min_savings: f64) -> bool {
	(out_bytes as f64) <= (src_bytes as f64) * (1.0 - min_savings)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn failed_verification() -> (Candidate, JobError) {
		(
			Candidate::Ravif(plan::ravif_settings(Speed::Fast)),
			ErrorKind::EncodeFailed("verification failed: the alpha channel is missing".into()).into(),
		)
	}

	#[test]
	fn a_failed_candidate_does_not_fail_the_job_when_another_is_usable() {
		let passed = Choice {
			best: Some((PathBuf::from("candidate1.jxl"), 100)),
			failures: vec![failed_verification()],
			..Choice::default()
		};
		let rejected = Choice {
			best_rejected: Some(70.0),
			failures: vec![failed_verification()],
			..Choice::default()
		};
		let only_failed = Choice {
			failures: vec![failed_verification()],
			..Choice::default()
		};

		assert!(matches!(verdict(passed, 80.0, 1000), Verdict::Output(_, 100)));
		assert!(matches!(
			verdict(rejected, 80.0, 1000),
			Verdict::Kept(KeepReason::QualityUnreachable(_))
		));
		assert!(matches!(
			verdict(only_failed, 80.0, 1000),
			Verdict::Failed(err) if matches!(err.kind, ErrorKind::EncodeFailed(_))
		));
		assert!(matches!(
			verdict(Choice::default(), 80.0, 1000),
			Verdict::Kept(KeepReason::NotSmaller { .. })
		));
	}

	#[test]
	fn a_passing_candidate_beats_a_short_one_and_the_note_follows_the_written_one() {
		let mut choice = Choice::default();

		choice.offer(PathBuf::from("candidate0.jxl"), 100, Some(78.0));
		let short_note = choice.short_note(85.0).unwrap();
		let open = choice.passing_size();
		choice.offer(PathBuf::from("candidate1.jxl"), 300, None);
		let passed = choice.best.clone();
		choice.offer(PathBuf::from("candidate2.jxl"), 50, Some(84.0));

		assert!(short_note.contains("SSIMULACRA2 78.0, target 85"), "{short_note}");
		assert_eq!(open, None, "a short best never stops a larger candidate");
		assert_eq!(passed, Some((PathBuf::from("candidate1.jxl"), 300)));
		assert_eq!(choice.best, passed);
		assert_eq!(choice.short_note(85.0), None);
		assert_eq!(choice.passing_size(), Some(300));
	}

	#[test]
	fn short_candidates_compete_on_score_and_passing_ones_on_size() {
		let mut short = Choice::default();
		let mut passing = Choice::default();

		short.offer(PathBuf::from("candidate0.avif"), 100, Some(70.0));
		short.offer(PathBuf::from("candidate1.jxl"), 200, Some(80.0));
		short.offer(PathBuf::from("candidate2.jxl"), 50, Some(75.0));
		passing.offer(PathBuf::from("candidate0.avif"), 300, None);
		passing.offer(PathBuf::from("candidate1.jxl"), 200, None);
		passing.offer(PathBuf::from("candidate2.jxl"), 250, None);

		assert_eq!(short.best, Some((PathBuf::from("candidate1.jxl"), 200)));
		assert!(short.short_note(85.0).unwrap().contains("SSIMULACRA2 80.0"));
		assert_eq!(passing.best, Some((PathBuf::from("candidate1.jxl"), 200)));
		assert_eq!(passing.short_note(85.0), None);
	}
}
