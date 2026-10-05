use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use tokio_util::sync::CancellationToken;

use crate::container::avif::GainMapInput;
use crate::decision::ImageDecision;
use crate::engine::{JobEvent, Stage};
use crate::format::ANIMATED_JXL;
use crate::images::animated::{self, Animation, ScanError};
use crate::images::avif::{self, AvifColour};
use crate::images::decode::{self, Decoded, SourceLayout, Undecodable};
use crate::images::jxl::{self, JxlJob, JxlPixels};
use crate::images::plan::{self, Candidate, Gate, ImageRoute, PrecheckInput, StillFacts, StillPlan};
use crate::images::props::{self, Effective};
use crate::images::verify::{self, Expected};
use crate::images::{blocking, gain_map, structure};
use crate::metadata::gain_map::{GainMapMetadata, GainMapSource};
use crate::metadata::icc::{self, Cicp};
use crate::metadata::{self, ImageFormat, MetadataMode, OutputMetadata, SourceMetadata, exif, sniff};
use crate::options::{Resize, WrittenBy, kept_as_requested, kept_as_requested_part, upscaled};
use crate::outcome::{ErrorKind, IoSide, JobError, KeepReason, Refusal};
use crate::preset::{Overrides, Speed};
use crate::probe::MediaInfo;
use crate::quality::keys::{CacheKey, Group, ImageGroup};
use crate::quality::memory::{Observation, QualityMemory};
use crate::toolchain::Toolchain;
use crate::video::run_ffmpeg;
use crate::workspace::Workspace;

const UPRIGHT_PNG: &str = "upright.png";

#[derive(Debug, Clone)]
pub struct ImageInput {
	pub bytes: Arc<Vec<u8>>,
	pub metadata: SourceMetadata,
	pub warnings: Vec<String>,
	pub pix_fmt: Option<String>,
	pub animated: bool,
	pub memory_bytes: u64,
	pub drop_aux: bool,
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
	/// `source_bytes` is a trim window's share of the source rather than its whole size.
	pub source_is_part: bool,
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
	pub resize: Option<Resize>,
}

#[derive(Debug)]
pub enum ImageOutcome {
	Output {
		path: PathBuf,
		bytes: u64,
		warnings: Vec<String>,
		/// Settings the groups learn once the output is committed.
		learned: Vec<(CacheKey, Observation)>,
		chosen: ImageDecision,
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
	gain_map: Option<CarriedGainMap>,
	warnings: Vec<String>,
}

/// The source's HDR gain map, turned and scaled with the image, that AVIF candidates carry.
struct CarriedGainMap {
	image: Arc<DynamicImage>,
	metadata: GainMapMetadata,
}

/// An AVIF candidate's parts when it carries a gain map, to rebuild it around a gain map encoded again.
struct GainMapParts {
	base: Vec<u8>,
	gain_map: Vec<u8>,
	quality: f32,
	speed: u8,
}

/// A candidate's gain map score after at most one encode at `gain_map::RAISED_QUALITY`.
struct GainMapCheck {
	score: f64,
	missed: bool,
	/// The candidate file's size after it was rebuilt around the gain map encoded again.
	rebuilt: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GainMapStep {
	Passed,
	Retry,
	Missed,
}

#[derive(Debug, Default)]
struct Choice {
	best: Option<(PathBuf, u64)>,
	/// The score of `best` when it misses the target and is written only because it was requested.
	best_short: Option<f64>,
	best_rejected: Option<f64>,
	/// The best gain map score of candidates that passed but whose HDR gain map missed its target.
	gain_map_missed: Option<f64>,
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
	/// A resized or retagged image written out losslessly for libjxl (`jxl::upright_png`).
	upright: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
	index: usize,
	best: Option<u64>,
	table: Option<Candidate>,
}

/// A candidate encode that scored at least the target, or is written short because it was requested.
struct Pass {
	path: PathBuf,
	size: u64,
	candidate: Candidate,
	score: f64,
	parts: Option<GainMapParts>,
	/// The file was rebuilt around its gain map encoded again.
	rebuilt: bool,
}

/// What became of the one lower encode after a pass well above the target.
enum StepDown {
	Passed(Pass),
	/// It scored below the target.
	Missed(Candidate),
	/// It wasn't tried, wasn't smaller, or couldn't be verified.
	Skipped,
}

#[derive(Debug)]
enum Tried {
	Passed {
		path: PathBuf,
		size: u64,
		candidate: Candidate,
		score: f64,
		gain_map: Option<f64>,
		short: bool,
		/// A lower encode of `candidate` that scored below the target.
		missed: Option<Candidate>,
	},
	Rejected {
		size: u64,
		score: f64,
	},
	TooLarge {
		size: u64,
		score: Option<f64>,
	},
	/// The image passed, but its HDR gain map missed `gain_map::TARGET`.
	GainMapMissed {
		size: u64,
		score: f64,
	},
	Failed(JobError),
}

impl Slot {
	fn beaten(self, size: u64) -> bool {
		self.best.is_some_and(|best| best <= size)
	}
}

impl Pass {
	fn passed(self, gain_map: Option<f64>, short: bool, missed: Option<Candidate>) -> Tried {
		Tried::Passed {
			path: self.path,
			size: self.size,
			candidate: self.candidate,
			score: self.score,
			gain_map,
			short,
			missed,
		}
	}
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
	resize: Option<Resize>,
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
	let dropped = match plan::precheck(&input) {
		Err(Gate::Damaged(reason)) => return Err(ErrorKind::SourceDamaged(reason).into()),
		Err(Gate::Keep(reason)) => return Ok(Err(reason)),
		Ok(dropped) => dropped,
	};

	let warnings = structure
		.warnings
		.iter()
		.chain(&metadata.warnings)
		.cloned()
		.chain(dropped)
		.collect();
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
		memory_bytes: dimensions.map_or(0, |dimensions| memory_bytes(dimensions, resize)),
		drop_aux,
	}))
}

/// The larger of the estimates at the source's size and at the resized one, whichever way the image is turned.
fn memory_bytes((width, height): (u32, u32), resize: Option<Resize>) -> u64 {
	let source = plan::worst_case_memory(width, height);
	let Some(resize) = resize else {
		return source;
	};

	[(width, height), (height, width)]
		.into_iter()
		.filter_map(|(width, height)| resize.size((f64::from(width), f64::from(height)), false).ok())
		.map(|(width, height)| plan::worst_case_memory(width, height))
		.fold(source, u64::max)
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
		orientation,
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

	let upright = image.dimensions();
	let (image, resize_warning) = match cx.resize {
		Some(resize) => resized(cx, image, resize).await?,
		None => (image, None),
	};

	let (width, height) = image.dimensions();
	let facts = StillFacts {
		width,
		height,
		effective,
		graphic,
		grey_icc: grey_profile(icc.as_deref()),
	};
	let mut plan = match plan::plan_still(&facts, cx.speed, &cx.overrides, cx.caps.libjxl, cx.route) {
		Ok(plan) => plan,
		Err(Refusal::Keep(reason)) => return Ok(Err(reason)),
		Err(Refusal::Impossible(reason)) => return Err(ErrorKind::EncodeFailed(reason).into()),
	};

	let mut gain_map = None;
	let mut gain_map_warning = None;
	if let Some(source) = &input.metadata.gain_map {
		let mut carrying = plan.clone();
		let refused = match plan::carry_gain_map(&mut carrying, &facts, cx.route) {
			Ok(true) => match fitted_gain_map(cx, input, source, orientation, upright, (width, height)).await? {
				Ok(carried) => {
					plan = carrying;
					gain_map = Some(carried);
					None
				}
				Err(err) => Some((
					KeepReason::Unsupported(format!("its HDR gain map can't be carried: {err}")),
					format!("the HDR gain map was dropped: {err}"),
				)),
			},
			Ok(false) => {
				gain_map_warning = Some(plan::GAIN_MAP_NOT_IN_JXL.to_owned());
				None
			}
			Err(reason) => Some((reason, plan::gain_map_not_in_avif(&facts))),
		};

		match refused {
			Some((reason, _)) if !input.drop_aux => return Ok(Err(reason)),
			Some((_, dropped)) => gain_map_warning = Some(dropped),
			None => {}
		}
	}

	let metadata = metadata::build_output(&input.metadata, cx.mode, width, height);
	let mut warnings = input.warnings.clone();
	warnings.extend(metadata.warnings.iter().cloned());
	if decode_errors > 0 {
		warnings.push(format!("{decode_errors} decoding errors were concealed"));
	}
	warnings.extend(resize_warning);
	warnings.extend(gain_map_warning);
	Ok(Ok(Still {
		image,
		icc,
		cicp,
		effective,
		graphic,
		plan,
		metadata,
		gain_map,
		warnings,
	}))
}

/// The source's gain map put through what the base went through: the base's orientation, then its resize from
/// `upright` to `size`.
async fn fitted_gain_map(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	source: &GainMapSource,
	orientation: u16,
	upright: (u32, u32),
	size: (u32, u32),
) -> Result<Result<CarriedGainMap, String>, JobError> {
	let (bytes, jpeg) = (Arc::clone(&input.bytes), source.clone());
	let photo_pixels = u64::from(upright.0) * u64::from(upright.1);
	let fitted = blocking(cx.cancel, move || {
		gain_map::decode(&bytes, &jpeg, photo_pixels).map(|gain| gain_map::fit(gain, orientation, upright, size))
	})
	.await?;

	Ok(fitted.map(|image| CarriedGainMap {
		image: Arc::new(image),
		metadata: source.metadata.clone(),
	}))
}

/// The upright image scaled with Lanczos, the same pixels every candidate encodes and is scored against, and the
/// upscale warning. The source's own traits (alpha, depth, graphic or photo) are measured before, on its pixels.
async fn resized(
	cx: &ImageContext<'_>,
	image: Arc<DynamicImage>,
	resize: Resize,
) -> Result<(Arc<DynamicImage>, Option<String>), JobError> {
	let (width, height) = image.dimensions();
	let shown = (f64::from(width), f64::from(height));
	let size = resize.size(shown, false).map_err(ErrorKind::EncodeFailed)?;
	if u64::from(size.0) * u64::from(size.1) > plan::MAX_VERIFY_PIXELS {
		return Err(ErrorKind::EncodeFailed(format!("{}×{} is too large to verify", size.0, size.1)).into());
	}

	let scaled = blocking(cx.cancel, move || scaled(&image, size)).await?;
	Ok((Arc::new(scaled), upscaled(shown, size)))
}

/// Lanczos on premultiplied colour: filtering straight alpha would pull the colour hidden under transparent pixels
/// (usually black) into every partly transparent edge.
fn scaled(image: &DynamicImage, (width, height): (u32, u32)) -> DynamicImage {
	if !image.color().has_alpha() {
		return image.resize_exact(width, height, FilterType::Lanczos3);
	}

	let mut premultiplied = image.to_rgba32f();
	for pixel in premultiplied.pixels_mut() {
		let alpha = pixel.0[3];
		for channel in &mut pixel.0[..3] {
			*channel *= alpha;
		}
	}

	let mut scaled = image::imageops::resize(&premultiplied, width, height, FilterType::Lanczos3);
	for pixel in scaled.pixels_mut() {
		let alpha = pixel.0[3].clamp(0.0, 1.0);
		pixel.0[3] = alpha;
		for channel in &mut pixel.0[..3] {
			*channel = if alpha > 0.0 {
				(*channel / alpha).clamp(0.0, 1.0)
			} else {
				0.0
			};
		}
	}

	let scaled = DynamicImage::ImageRgba32F(scaled);
	match image {
		DynamicImage::ImageLumaA8(_) => DynamicImage::ImageLumaA8(scaled.to_luma_alpha8()),
		DynamicImage::ImageLumaA16(_) => DynamicImage::ImageLumaA16(scaled.to_luma_alpha16()),
		DynamicImage::ImageRgba16(_) => DynamicImage::ImageRgba16(scaled.to_rgba16()),
		DynamicImage::ImageRgba32F(_) => scaled,
		_ => DynamicImage::ImageRgba8(scaled.to_rgba8()),
	}
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
			gain_map: still.gain_map.as_ref().map(|carried| &carried.metadata),
		},
		reference: eight_bit_reference(cx, &still).await?,
		upright: upright_for_libjxl(cx, workspace, &still).await?,
	};
	let mut choice = Choice::default();
	let mut learned = Vec::new();
	let mut picked = None;

	for (index, planned) in still.plan.candidates.iter().copied().enumerate() {
		let key = candidate_key(cx.tools, input, &still, planned);
		let candidate = key
			.as_ref()
			.and_then(|key| cx.memory.lookup(key))
			.map_or(planned, |stats| {
				planned.with_setting(cx.speed, still.plan.target, stats.setting)
			});
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
				gain_map,
				short,
				missed,
			} => {
				choice.note(size);
				let offered = path.clone();
				choice.offer(path, size, short.then_some(score));
				if choice.best.as_ref().is_some_and(|(best, _)| *best == offered) {
					picked = Some(candidate_decision(candidate, score, still.plan.target, gain_map));
				}

				if !short
					&& cx.resize.is_none()
					&& let Some(key) = key
					&& let Some(setting) = candidate.needed_setting(score, still.plan.target, cx.speed)
				{
					let setting = missed.map_or(setting, |missed| candidate.learned_past_miss(missed, setting));
					learned.push((
						key,
						Observation {
							setting,
							slope: None,
							bpp: None,
							offset: None,
							without_grain: false,
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
			Tried::GainMapMissed { size, score } => {
				choice.note(size);
				choice.gain_map_missed = Some(choice.gain_map_missed.map_or(score, |best| best.max(score)));
			}
			Tried::Failed(err) => choice.failures.push((candidate, err)),
		}
	}

	let mut notes: Vec<String> = choice.short_note(still.plan.target).into_iter().collect();
	if cx.forced
		&& let Some((_, size)) = &choice.best
		&& !saves_enough(cx.source_bytes, *size, cx.min_savings)
	{
		notes.push(kept_as_requested(cx.source_bytes, *size, WrittenBy::Request));
	}

	notes.extend(gain_map_short_note(picked.as_ref()));

	let mut warnings = still.warnings;
	warnings.extend(notes);
	finish_choice(cx, workspace, choice, still.plan.target, warnings, learned, picked)
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
		let (bytes, parts) = encode_candidate(cx, input, workspace, still, env, candidate).await?;
		fs::write(&path, &bytes).map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;

		let size = bytes.len() as u64;
		if too_large(cx, size)
			&& last_score.is_none()
			&& let Some(fallback) = table.take()
		{
			candidate = fallback;
			continue;
		}

		if too_large(cx, size) || slot.beaten(size) {
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

		let short = if score >= still.plan.target {
			false
		} else {
			match candidate
				.raised(score, still.plan.target)
				.filter(|_| retries < plan::MAX_RETRIES)
			{
				Some(raised) => {
					candidate = raised;
					retries += 1;
					last_score = Some(score);
					continue;
				}
				None if cx.forced => true,
				None => return Ok(Tried::Rejected { size, score }),
			}
		};

		let mut pass = Pass {
			path,
			size,
			candidate,
			score,
			parts,
			rebuilt: false,
		};
		let mut missed = None;
		if !short {
			match Box::pin(step_down(cx, input, workspace, still, env, slot, &pass)).await? {
				StepDown::Passed(lowered) => {
					match Box::pin(either_pass(cx, workspace, still, env, slot, pass, lowered)).await? {
						Ok(tried) => return Ok(tried),
						Err(earlier) => pass = earlier,
					}
				}
				StepDown::Missed(lowered) => missed = Some(lowered),
				StepDown::Skipped => {}
			}
		}

		return Ok(
			match Box::pin(gain_map_checked(cx, workspace, still, env, slot, &mut pass)).await? {
				Ok(gain_map) => pass.passed(gain_map, short, missed),
				Err(tried) => tried,
			},
		);
	}
}

fn too_large(cx: &ImageContext<'_>, size: u64) -> bool {
	!cx.forced && !saves_enough(cx.source_bytes, size, cx.min_savings)
}

/// Encodes a pass well above the target once more, aimed just above it. A lower encode that isn't passed on is removed;
/// the earlier pass is left to the caller.
async fn step_down(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	workspace: &Workspace,
	still: &Still,
	env: &Env<'_>,
	slot: Slot,
	pass: &Pass,
) -> Result<StepDown, JobError> {
	let target = still.plan.target;
	let Some(lowered) = pass.candidate.stepped_down(pass.score, target, cx.speed) else {
		return Ok(StepDown::Skipped);
	};

	(cx.on_event)(JobEvent::Stage(Stage::Encode));
	let path = workspace.file(&format!("candidate{}-lower.{}", slot.index, lowered.extension()));
	let (bytes, parts) = encode_candidate(cx, input, workspace, still, env, lowered).await?;
	fs::write(&path, &bytes).map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;

	let size = bytes.len() as u64;
	let score = if size < pass.size && !slot.beaten(size) {
		(cx.on_event)(JobEvent::Stage(Stage::Verify));
		match verify_candidate(cx, still, env, lowered, &path, bytes).await {
			Ok(score) => Some(score),
			Err(err) if matches!(err.kind, ErrorKind::EncodeFailed(_)) => None,
			Err(err) => return Err(err),
		}
	} else {
		None
	};

	match score {
		Some(score) if score >= target => Ok(StepDown::Passed(Pass {
			path,
			size,
			candidate: lowered,
			score,
			parts,
			rebuilt: false,
		})),
		missed => {
			let _ = fs::remove_file(&path);
			Ok(missed.map_or(StepDown::Skipped, |_| StepDown::Missed(lowered)))
		}
	}
}

/// The lower encode once its gain map passed, or, when that gain map had to be rebuilt, whichever of the two passes is
/// smaller after both gain maps were checked. `Err` gives back the earlier pass, unchecked, when the lower encode's gain
/// map failed it. The file not kept is removed.
async fn either_pass(
	cx: &ImageContext<'_>,
	workspace: &Workspace,
	still: &Still,
	env: &Env<'_>,
	slot: Slot,
	mut earlier: Pass,
	mut lowered: Pass,
) -> Result<Result<Tried, Pass>, JobError> {
	let Ok(gain_map) = Box::pin(gain_map_checked(cx, workspace, still, env, slot, &mut lowered)).await? else {
		let _ = fs::remove_file(&lowered.path);
		return Ok(Err(earlier));
	};

	if lowered.rebuilt
		&& let Ok(earlier_gain_map) = Box::pin(gain_map_checked(cx, workspace, still, env, slot, &mut earlier)).await?
		&& earlier.size <= lowered.size
	{
		let _ = fs::remove_file(&lowered.path);
		return Ok(Ok(earlier.passed(earlier_gain_map, false, None)));
	}

	let _ = fs::remove_file(&earlier.path);
	Ok(Ok(lowered.passed(gain_map, false, None)))
}

/// The score of the HDR gain map `pass` carries, if any, or what the candidate comes to when the gain map missed or
/// the file rebuilt around it no longer fits.
async fn gain_map_checked(
	cx: &ImageContext<'_>,
	workspace: &Workspace,
	still: &Still,
	env: &Env<'_>,
	slot: Slot,
	pass: &mut Pass,
) -> Result<Result<Option<f64>, Tried>, JobError> {
	let Some(parts) = pass.parts.take() else {
		return Ok(Ok(None));
	};

	let checked = match Box::pin(check_gain_map(cx, workspace, still, env, &pass.path, parts)).await {
		Ok(checked) => checked,
		Err(err) if matches!(err.kind, ErrorKind::EncodeFailed(_)) => return Ok(Err(Tried::Failed(err))),
		Err(err) => return Err(err),
	};
	pass.size = checked.rebuilt.unwrap_or(pass.size);
	pass.rebuilt = checked.rebuilt.is_some();

	if checked.missed && !cx.forced {
		return Ok(Err(Tried::GainMapMissed {
			size: pass.size,
			score: checked.score,
		}));
	}

	if checked.rebuilt.is_some() && (too_large(cx, pass.size) || slot.beaten(pass.size)) {
		return Ok(Err(Tried::TooLarge {
			size: pass.size,
			score: None,
		}));
	}

	Ok(Ok(Some(checked.score)))
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

async fn upright_for_libjxl(
	cx: &ImageContext<'_>,
	workspace: &Workspace,
	still: &Still,
) -> Result<Option<PathBuf>, JobError> {
	let libjxl = still
		.plan
		.candidates
		.iter()
		.any(|candidate| !matches!(candidate, Candidate::Ravif(_)));
	let retagged = still.icc.is_none() && still.cicp.is_some_and(|cicp| jxl::libjxl_cicp(cicp) != cicp);
	if (cx.resize.is_none() && !retagged) || !libjxl {
		return Ok(None);
	}

	let image = Arc::clone(&still.image);
	let pixels = JxlPixels::for_source(still.effective, grey_profile(still.icc.as_deref()));
	let (icc, cicp) = (still.icc.clone(), still.cicp);
	let png = blocking(cx.cancel, move || jxl::upright_png(&image, pixels, icc, cicp))
		.await?
		.map_err(ErrorKind::EncodeFailed)?;

	let path = workspace.file(UPRIGHT_PNG);
	fs::write(&path, png).map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;
	Ok(Some(path))
}

async fn encode_candidate(
	cx: &ImageContext<'_>,
	input: &ImageInput,
	workspace: &Workspace,
	still: &Still,
	env: &Env<'_>,
	candidate: Candidate,
) -> Result<(Vec<u8>, Option<GainMapParts>), JobError> {
	match candidate {
		Candidate::Ravif(settings) => {
			let pixels = Arc::clone(env.reference.as_ref().expect("prepared for AVIF candidates"));
			let (alpha, threads) = (still.effective.alpha, cx.threads);
			let encoded = blocking(cx.cancel, move || avif::encode(&pixels, alpha, settings, threads))
				.await?
				.map_err(ErrorKind::EncodeFailed)?;

			let Some(carried) = &still.gain_map else {
				let bytes =
					avif::finish(&encoded, env.colour, &still.metadata, None).map_err(ErrorKind::EncodeFailed)?;
				return Ok((bytes, None));
			};

			let gain_map = encode_gain_map(cx, carried, settings.quality, settings.speed).await?;
			let bytes = with_gain_map(&encoded, &gain_map, still, env)?;
			let parts = GainMapParts {
				base: encoded,
				gain_map,
				quality: settings.quality,
				speed: settings.speed,
			};
			Ok((bytes, Some(parts)))
		}
		Candidate::JxlLossy(_) | Candidate::JxlLossless(_) => {
			let (source, layout) = match env.upright.as_deref() {
				Some(upright) => (upright, jxl::UPRIGHT),
				None => (cx.source, decode::layout(&input.bytes, &input.metadata)),
			};
			let job = jxl_job(source, layout, still, candidate, cx.threads);
			let bytes = jxl::encode(
				&cx.tools.ffmpeg,
				&job,
				&workspace.file("libjxl.jxl"),
				&still.metadata,
				cx.cancel,
			)
			.await?;
			Ok((bytes, None))
		}
	}
}

async fn encode_gain_map(
	cx: &ImageContext<'_>,
	carried: &CarriedGainMap,
	quality: f32,
	speed: u8,
) -> Result<Vec<u8>, JobError> {
	let (gain, threads) = (Arc::clone(&carried.image), cx.threads);
	Ok(
		blocking(cx.cancel, move || gain_map::encode(&gain, quality, speed, threads))
			.await?
			.map_err(ErrorKind::EncodeFailed)?,
	)
}

fn with_gain_map(base: &[u8], gain_map: &[u8], still: &Still, env: &Env<'_>) -> Result<Vec<u8>, JobError> {
	let carried = still.gain_map.as_ref().expect("only called with a carried gain map");
	let input = GainMapInput {
		encoded: gain_map,
		metadata: &carried.metadata,
	};
	Ok(avif::finish(base, env.colour, &still.metadata, Some(&input)).map_err(ErrorKind::EncodeFailed)?)
}

/// Scores the candidate's gain map; one that misses `gain_map::TARGET` below `gain_map::RAISED_QUALITY` is encoded
/// once more at it, and the file at `path` rebuilt around the new gain map.
async fn check_gain_map(
	cx: &ImageContext<'_>,
	workspace: &Workspace,
	still: &Still,
	env: &Env<'_>,
	path: &Path,
	parts: GainMapParts,
) -> Result<GainMapCheck, JobError> {
	let carried = still.gain_map.as_ref().expect("only called with a carried gain map");
	let score = gain_map::score(cx.tools, workspace, &parts.gain_map, &carried.image, cx.cancel).await?;
	let step = gain_map_step(score, parts.quality);
	if step != GainMapStep::Retry {
		return Ok(GainMapCheck {
			score,
			missed: step == GainMapStep::Missed,
			rebuilt: None,
		});
	}

	let gain_map = encode_gain_map(cx, carried, gain_map::RAISED_QUALITY, parts.speed).await?;
	let bytes = with_gain_map(&parts.base, &gain_map, still, env)?;
	fs::write(path, &bytes).map_err(|err| JobError::io(IoSide::Destination(path), &err))?;
	verify::check_avif(&bytes, &env.expected)?;

	let score = gain_map::score(cx.tools, workspace, &gain_map, &carried.image, cx.cancel).await?;
	Ok(GainMapCheck {
		score,
		missed: gain_map_step(score, gain_map::RAISED_QUALITY) == GainMapStep::Missed,
		rebuilt: Some(bytes.len() as u64),
	})
}

fn gain_map_step(score: f64, quality: f32) -> GainMapStep {
	if score >= gain_map::TARGET {
		GainMapStep::Passed
	} else if quality < gain_map::RAISED_QUALITY {
		GainMapStep::Retry
	} else {
		GainMapStep::Missed
	}
}

/// The note for a requested output written although its HDR gain map missed its target.
fn gain_map_short_note(picked: Option<&ImageDecision>) -> Option<String> {
	let score = picked?.gain_map.filter(|score| *score < gain_map::TARGET)?;
	Some(format!(
		"the HDR gain map scored {score:.1}, below {:.0}; it was written because it was requested",
		gain_map::TARGET
	))
}

fn jxl_job<'a>(
	source: &'a Path,
	layout: SourceLayout,
	still: &Still,
	candidate: Candidate,
	threads: usize,
) -> JxlJob<'a> {
	let settings = match candidate {
		Candidate::JxlLossy(settings) | Candidate::JxlLossless(settings) => settings,
		Candidate::Ravif(_) => unreachable!("only JPEG XL candidates run through libjxl"),
	};
	JxlJob {
		source,
		layout,
		settings,
		pixels: JxlPixels::for_source(still.effective, grey_profile(still.icc.as_deref())),
		threads,
	}
}

fn finish_choice(
	cx: &ImageContext<'_>,
	workspace: &Workspace,
	choice: Choice,
	target: f64,
	mut warnings: Vec<String>,
	learned: Vec<(CacheKey, Observation)>,
	picked: Option<ImageDecision>,
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
				chosen: picked.unwrap_or_default(),
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

	if let Some(score) = choice.gain_map_missed {
		return Verdict::Kept(KeepReason::QualityUnreachable(format!(
			"its HDR gain map scored {score:.1}, below {:.0}",
			gain_map::TARGET
		)));
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

fn candidate_decision(candidate: Candidate, score: f64, target: f64, gain_map: Option<f64>) -> ImageDecision {
	let (route, setting) = match candidate {
		Candidate::Ravif(settings) => ("AVIF", Some(format!("quality {}", settings.quality))),
		Candidate::JxlLossy(settings) => ("JPEG XL", Some(format!("distance {:.2}", settings.distance))),
		Candidate::JxlLossless(_) => ("lossless JPEG XL", None),
	};
	ImageDecision {
		route: route.to_owned(),
		setting,
		score: Some(score),
		target: Some(target),
		gain_map,
	}
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

	if !cx.caps.libaom {
		return Err(ErrorKind::Toolchain(animated::NEEDS_LIBAOM.into()).into());
	}

	let shown = (f64::from(timeline.width), f64::from(timeline.height));
	let size = cx
		.resize
		.map(|resize| resize.size(shown, false))
		.transpose()
		.map_err(ErrorKind::EncodeFailed)?;

	let animation = Animation {
		alpha: animated::has_alpha(cx.tools, cx.source, cx.threads, cx.cancel).await?,
		timeline,
		size,
		matrix: animated::IMAGE_MATRIX.to_owned(),
	};

	let mut warnings = animation_warnings(cx, input);
	warnings.extend(size.and_then(|size| upscaled(shown, size)));

	Box::pin(encode_animation(cx, cx.source, &animation, workspace, warnings))
		.await
		.map(Some)
}

/// Encodes `frames` (an animated image, or the frames prepared from a video) as an animated AVIF and checks it
/// against them. `cx.source_bytes` is what the output is weighed against.
pub async fn encode_animation(
	cx: &ImageContext<'_>,
	frames: &Path,
	animation: &Animation,
	workspace: &Workspace,
	mut warnings: Vec<String>,
) -> Result<ImageOutcome, JobError> {
	let settings = animated::plan_animation(cx.speed);

	(cx.on_event)(JobEvent::Stage(Stage::Encode));
	let output = workspace.file("out.avif");
	let cmd = animated::encode_command(&cx.tools.ffmpeg, frames, &output, animation, settings, cx.threads);
	let total = f64::from(animation.timeline.frames);
	run_ffmpeg(
		&cmd,
		|progress| {
			if let Some(frame) = progress.frame {
				#[allow(clippy::cast_precision_loss)]
				(cx.on_event)(JobEvent::Progress((frame as f64 / total).clamp(0.0, 1.0)));
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
			return Ok(ImageOutcome::Kept(KeepReason::NotSmaller {
				src_bytes: cx.source_bytes,
				out_bytes: bytes,
			}));
		}

		warnings.push(if cx.source_is_part {
			kept_as_requested_part(cx.source_bytes, bytes, WrittenBy::Request)
		} else {
			kept_as_requested(cx.source_bytes, bytes, WrittenBy::Request)
		});
	}

	(cx.on_event)(JobEvent::Stage(Stage::Verify));
	let score = animated::verify_animation(cx.tools, frames, &output, animation, cx.threads, cx.cancel).await?;
	let target = plan::image_target(cx.speed, &cx.overrides);
	if score < target {
		if !cx.forced {
			return Ok(ImageOutcome::Kept(KeepReason::QualityUnreachable(format!(
				"quality target not reached at a smaller size (SSIMULACRA2 {score:.1}, target {target:.0})"
			))));
		}

		warnings.push(short_warning(score, target));
	}

	for warning in &warnings {
		(cx.on_event)(JobEvent::Warning(warning.clone()));
	}
	Ok(ImageOutcome::Output {
		path: output,
		bytes,
		warnings,
		learned: Vec::new(),
		chosen: ImageDecision {
			route: "animated AVIF".to_owned(),
			setting: Some(format!("CRF {}", settings.crf)),
			score: Some(score),
			target: Some(target),
			gain_map: None,
		},
	})
}

fn animation_warnings(cx: &ImageContext<'_>, input: &ImageInput) -> Vec<String> {
	let mut warnings = input.warnings.clone();
	let personal = input.metadata.exif.is_some() || input.metadata.xmp.is_some() || !input.metadata.comments.is_empty();
	if cx.mode == MetadataMode::Full && personal {
		warnings.push("metadata is not carried into animated AVIF yet".into());
	}
	if input.metadata.icc.is_some() {
		warnings.push("the colour profile is not carried into animated AVIF yet".into());
	}
	warnings
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

	/// 64×64, the left half opaque red and the right half transparent black, as most tools save transparency.
	fn red_edge() -> image::RgbaImage {
		image::RgbaImage::from_fn(64, 64, |x, _| {
			if x < 32 {
				image::Rgba([255, 0, 0, 255])
			} else {
				image::Rgba([0, 0, 0, 0])
			}
		})
	}

	#[test]
	fn partly_transparent_edges_keep_their_colour_when_scaled() {
		let grey = DynamicImage::ImageLumaA16(DynamicImage::ImageRgba8(red_edge()).to_luma_alpha16());
		let level = grey.to_luma_alpha16().get_pixel(0, 0).0[0];

		let red = scaled(&DynamicImage::ImageRgba8(red_edge()), (22, 22));
		let grey = scaled(&grey, (22, 22));
		let opaque = scaled(&DynamicImage::ImageRgb8(image::RgbImage::new(4, 4)), (2, 2));

		let DynamicImage::ImageRgba8(red) = red else {
			panic!("{red:?}")
		};
		let edge: Vec<[u8; 4]> = red
			.pixels()
			.map(|pixel| pixel.0)
			.filter(|pixel| pixel[3] >= 8)
			.collect();
		assert!(
			edge.iter().any(|pixel| pixel[3] < 247),
			"no partly transparent pixel: {edge:?}"
		);
		for pixel in &edge {
			assert!(pixel[0] >= 250 && pixel[1] <= 5 && pixel[2] <= 5, "{pixel:?}");
		}

		let DynamicImage::ImageLumaA16(grey) = grey else {
			panic!("{grey:?}")
		};
		for pixel in grey.pixels().filter(|pixel| pixel.0[1] >= 2048) {
			assert!(pixel.0[0].abs_diff(level) <= 512, "{pixel:?}, level {level}");
		}
		assert!(matches!(opaque, DynamicImage::ImageRgb8(_)));
	}

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
	fn a_gain_map_that_misses_keeps_the_original_unless_another_candidate_was_written() {
		let missed = Choice {
			gain_map_missed: Some(65.04),
			smallest: Some(500),
			..Choice::default()
		};
		let missed_and_written = Choice {
			best: Some((PathBuf::from("candidate1.avif"), 400)),
			gain_map_missed: Some(65.0),
			..Choice::default()
		};

		assert!(matches!(
			verdict(missed, 80.0, 1000),
			Verdict::Kept(KeepReason::QualityUnreachable(reason)) if reason == "its HDR gain map scored 65.0, below 70"
		));
		assert!(matches!(
			verdict(missed_and_written, 80.0, 1000),
			Verdict::Output(_, 400)
		));
	}

	#[test]
	fn a_gain_map_below_its_target_is_encoded_again_only_below_the_raised_quality() {
		assert_eq!(gain_map_step(gain_map::TARGET, 60.0), GainMapStep::Passed);
		assert_eq!(gain_map_step(85.0, 95.0), GainMapStep::Passed);
		assert_eq!(gain_map_step(65.0, 88.0), GainMapStep::Retry);
		assert_eq!(gain_map_step(65.0, gain_map::RAISED_QUALITY), GainMapStep::Missed);
		assert_eq!(gain_map_step(65.0, 95.0), GainMapStep::Missed);
	}

	#[test]
	fn a_requested_output_with_a_gain_map_below_its_target_says_so() {
		let decision = |gain_map| ImageDecision {
			gain_map,
			..ImageDecision::default()
		};

		assert_eq!(
			gain_map_short_note(Some(&decision(Some(64.96)))).as_deref(),
			Some("the HDR gain map scored 65.0, below 70; it was written because it was requested")
		);
		assert_eq!(gain_map_short_note(Some(&decision(Some(70.0)))), None);
		assert_eq!(gain_map_short_note(Some(&decision(None))), None);
		assert_eq!(gain_map_short_note(None), None);
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
