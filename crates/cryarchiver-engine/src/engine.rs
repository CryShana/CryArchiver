use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::audio::plan::{AudioPlan, AudioRules, FilePlan, plan_file_with, retarget};
use crate::audio::{cover, encode, verify};
use crate::commit::{Artifact, CommitError, OutputSink, SourceTimes, check_space, free_space};
use crate::container::{Container, Cover, MATROSKA_VIDEO, MP4_VIDEO};
use crate::detect::{self, Detection, MediaClass};
use crate::format::{OutputFormat, check_kind};
use crate::images::job::{self as image_job, ImageCaps, ImageContext, ImageInput, ImageOutcome};
use crate::images::plan::ImageRoute;
use crate::integrity;
use crate::metadata::MetadataMode;
use crate::options::{JobOptions, TrimWindow, kept_as_requested};
use crate::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason, Refusal};
use crate::preset::{Overrides, Speed};
use crate::probe::{self, Frame, MediaInfo, ProbeError, Stream};
use crate::quality::keys::{CacheKey, Group, VideoGroup};
use crate::quality::memory::{GroupStats, Observation, QualityMemory, SharedMemory};
use crate::quality::search::{self, DEFAULT_SLOPE, MAX_PROBES, Target};
use crate::text::display_path;
use crate::toolchain::{LIBAOM, LIBVPX_VP9, MediaKind, SVT_AV1, Toolchain};
use crate::video::analyze::Analysis;
use crate::video::encode::{EncodedVideo, Sample};
use crate::video::mux::{self, Attachment, ChapterSource, MuxInputs, MuxPlan};
use crate::video::plan::{self as video_plan, Encoder, VideoCaps, VideoPlan, plan_video, precheck, usable_main};
use crate::video::quality::{self as video_quality, MAX_RETRIES, Scorer, Searched, Strategy};
use crate::video::streams::{Extracted, StreamPlan, StreamRules, plan_streams};
use crate::video::verify::{
	Expected, Timelines, alpha_ssim, check_alpha, earliest_video_start, probe_temp, start_offsets, verify_output,
};
use crate::video::{analyze, chapters, encode as video_encode};
use crate::workspace::Workspace;

const SOURCE_METADATA: &str = "source-metadata.json";
const CHAPTER_FILE: &str = "chapters.txt";
const MEMORY_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub struct Job {
	pub source: PathBuf,
	pub speed: Speed,
	pub keep_metadata: bool,
	pub drop_aux: bool,
	pub min_savings: f64,
	pub overrides: Overrides,
	pub options: JobOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
	Detect,
	Probe,
	Plan,
	Queued,
	WaitingForMemory,
	Analyze,
	Sample,
	Encode,
	Mux,
	Metadata,
	Verify,
	Commit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobEvent {
	Stage(Stage),
	Progress(f64),
	Warning(String),
}

/// The engine's job call as an object, so the project layer can run without ffmpeg in its tests.
pub trait Encode: Send + Sync {
	fn encode<'a>(
		&'a self,
		job: &'a Job,
		sink: &'a dyn OutputSink,
		on_event: &'a (dyn Fn(JobEvent) + Sync),
		cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = JobOutcome> + Send + 'a>>;
}

/// Admission by estimated peak memory (project layer spec §8.2). The engine asks before a job's heavy work starts,
/// asks again every quarter second until the reservation succeeds, and releases it when the job ends.
pub trait MemoryGate: Send + Sync {
	fn try_reserve(&self, bytes: u64) -> bool;

	fn release(&self, bytes: u64);
}

struct MemoryReservation<'a> {
	gate: &'a dyn MemoryGate,
	bytes: u64,
}

impl Drop for MemoryReservation<'_> {
	fn drop(&mut self) {
		self.gate.release(self.bytes);
	}
}

pub struct Engine {
	tools: Toolchain,
	video_slots: Semaphore,
	image_threads: usize,
	metric_threads: usize,
	memory_gate: Option<Arc<dyn MemoryGate>>,
	quality: Arc<dyn QualityMemory>,
}

struct Context<'a> {
	job: &'a Job,
	source_bytes: u64,
	on_event: &'a (dyn Fn(JobEvent) + Sync),
	cancel: &'a CancellationToken,
}

struct Staged {
	path: PathBuf,
	bytes: u64,
	warnings: Vec<String>,
	/// Recorded only once the output is committed.
	learned: Vec<(CacheKey, Observation)>,
}

enum Produced {
	Output(Staged),
	Kept(KeepReason),
}

enum Planned {
	Audio {
		info: Box<MediaInfo>,
		plan: FilePlan,
		from_video: bool,
	},
	Video,
	Image(Box<ImageInput>),
}

struct VideoJob<'a> {
	info: &'a MediaInfo,
	probe_json: &'a [u8],
	first_frame: Option<&'a Frame>,
	main: &'a Stream,
	video: &'a VideoPlan,
	streams: &'a StreamPlan,
	container: Container,
}

struct Temp {
	path: PathBuf,
	info: MediaInfo,
}

struct EncodedTemps {
	video: Temp,
	audio: Option<Temp>,
	encoded: EncodedVideo,
	warnings: Vec<String>,
}

struct VideoInputs<'a> {
	info: &'a MediaInfo,
	streams: &'a StreamPlan,
	declared_s: Option<f64>,
	span: Option<TrimWindow>,
}

struct VideoQuality {
	key: CacheKey,
	stats: Option<GroupStats>,
	target: Target,
	strategy: Strategy,
	slope: f64,
}

impl VideoQuality {
	fn start(&self) -> f64 {
		match self.strategy {
			Strategy::Search { start } => start,
			Strategy::Fixed { crf, .. } => crf,
		}
	}
}

enum Tuning {
	Keep(KeepReason),
	Encode { verify: bool },
}

enum Checked {
	Passed(Observation),
	Missed(KeepReason),
	/// Written as requested; learns nothing.
	Short,
}

impl Engine {
	pub fn new(tools: Toolchain) -> Self {
		let cores = std::thread::available_parallelism().map_or(1, usize::from);
		Self {
			tools,
			video_slots: Semaphore::new(1),
			image_threads: cores,
			metric_threads: cores,
			memory_gate: None,
			quality: Arc::new(SharedMemory::default()),
		}
	}

	#[must_use]
	pub fn with_quality_memory(self, quality: Arc<dyn QualityMemory>) -> Self {
		Self { quality, ..self }
	}

	#[must_use]
	pub fn with_memory_gate(self, gate: Arc<dyn MemoryGate>) -> Self {
		Self {
			memory_gate: Some(gate),
			..self
		}
	}

	#[must_use]
	pub fn with_video_jobs(self, jobs: usize) -> Self {
		Self {
			video_slots: Semaphore::new(jobs.max(1)),
			..self
		}
	}

	#[must_use]
	pub fn with_image_threads(self, threads: usize) -> Self {
		Self {
			image_threads: threads.max(1),
			..self
		}
	}

	pub async fn run(
		&self,
		job: &Job,
		sink: &dyn OutputSink,
		on_event: &(dyn Fn(JobEvent) + Sync),
		cancel: &CancellationToken,
	) -> JobOutcome {
		Box::pin(self.process(job, sink, on_event, cancel))
			.await
			.unwrap_or_else(JobOutcome::Failed)
	}

	#[allow(clippy::too_many_lines)]
	async fn process(
		&self,
		job: &Job,
		sink: &dyn OutputSink,
		on_event: &(dyn Fn(JobEvent) + Sync),
		cancel: &CancellationToken,
	) -> Result<JobOutcome, JobError> {
		if cancel.is_cancelled() {
			return Err(ErrorKind::Cancelled.into());
		}

		on_event(JobEvent::Stage(Stage::Detect));
		let Detection { meta, class, probe_log } = detect::detect(&self.tools, &job.source, cancel).await?;
		if let (Some(format), Some(kind)) = (job.options.format, media_kind(&class)) {
			check_kind(format, kind).map_err(ErrorKind::EncodeFailed)?;
		}

		let animated = matches!(class, MediaClass::AnimatedImage(_));
		let planned = match class {
			MediaClass::Audio(info) => {
				self.require(MediaKind::Audio)?;
				on_event(JobEvent::Stage(Stage::Plan));
				let rules = AudioRules {
					forced: job.options.forced(),
					reencode: job.options.trim.is_some(),
				};
				match plan_file_with(&info, job.speed, &job.overrides, rules) {
					AudioPlan::Encode(plan) => {
						let mut plan = retarget(plan, job.options.format, &info).map_err(ErrorKind::EncodeFailed)?;
						plan.options.strip = job.options.strip;
						plan.options.window = trim_window(job, info.format.duration)?;

						Planned::Audio {
							plan,
							info: Box::new(info),
							from_video: false,
						}
					}
					AudioPlan::Keep(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
				}
			}
			MediaClass::Video(info)
				if job
					.options
					.format
					.is_some_and(|format| format.kind() == MediaKind::Audio) =>
			{
				self.require(MediaKind::Audio)?;
				check_damage(&job.source)?;

				on_event(JobEvent::Stage(Stage::Plan));
				let rules = AudioRules {
					forced: true,
					reencode: job.options.trim.is_some(),
				};
				match plan_file_with(&info, job.speed, &job.overrides, rules) {
					AudioPlan::Encode(plan) => {
						let mut plan =
							extracted(retarget(plan, job.options.format, &info).map_err(ErrorKind::EncodeFailed)?);
						plan.options.strip = job.options.strip;
						plan.options.window = trim_window(job, info.format.duration)?;

						Planned::Audio {
							plan,
							info: Box::new(info),
							from_video: true,
						}
					}
					AudioPlan::Keep(reason) => return Err(ErrorKind::EncodeFailed(extraction_refusal(&reason)).into()),
				}
			}
			MediaClass::Video(info) => {
				self.require(MediaKind::Video)?;
				check_damage(&job.source)?;

				on_event(JobEvent::Stage(Stage::Plan));
				let rules = stream_rules(job);
				let planned = self
					.video_main(&info, job.options.forced())
					.map_err(Refusal::from)
					.and_then(|_| plan_streams(&info, &rules));
				match planned {
					Ok(_) => Planned::Video,
					Err(Refusal::Keep(reason)) => return Ok(JobOutcome::KeptOriginal { reason }),
					Err(Refusal::Impossible(reason)) => return Err(ErrorKind::EncodeFailed(reason).into()),
				}
			}
			MediaClass::StillImage(info) | MediaClass::AnimatedImage(info) => {
				if job.options.trim.is_some() {
					return Err(
						ErrorKind::EncodeFailed("--ss and --to apply to audio and video, not images".into()).into(),
					);
				}

				self.require(MediaKind::Image)?;
				on_event(JobEvent::Stage(Stage::Plan));
				match image_job::prepare(&job.source, &info, animated, job.drop_aux, metadata_mode(job))? {
					Ok(input) => Planned::Image(Box::new(input)),
					Err(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
				}
			}
			MediaClass::Kept(reason) => {
				return Ok(JobOutcome::KeptOriginal {
					reason: KeepReason::Unsupported(reason),
				});
			}
			MediaClass::Damaged(reason) => return Err(JobError::with_log(ErrorKind::SourceDamaged(reason), probe_log)),
			MediaClass::Other(reason) => return Ok(JobOutcome::Skipped { reason }),
		};

		let _memory = match &planned {
			Planned::Image(input) => self.reserve_memory(input.memory_bytes, on_event, cancel).await?,
			Planned::Audio { .. } | Planned::Video => None,
		};

		let before =
			SourceTimes::from_metadata(&meta).map_err(|err| JobError::io(IoSide::Source(&job.source), &err))?;
		let work_root = sink.work_root(&job.source);
		// Must outlive `finish`: dropping the workspace deletes the staged output.
		let workspace = open_workspace(sink, &work_root, &job.source, meta.len())?;

		let cx = Context {
			job,
			source_bytes: meta.len(),
			on_event,
			cancel,
		};
		let produced = match &planned {
			Planned::Audio { info, plan, from_video } => {
				self.produce_audio(&cx, info, plan, *from_video, &workspace).await
			}
			Planned::Video => Box::pin(self.produce_video(&cx, &workspace)).await,
			Planned::Image(input) => Box::pin(self.produce_image(&cx, input, &workspace)).await,
		};
		let mut staged = match produced.map_err(|err| disk_full(err, &work_root, cx.source_bytes))? {
			Produced::Output(staged) => staged,
			Produced::Kept(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
		};
		let learned = std::mem::take(&mut staged.learned);

		on_event(JobEvent::Stage(Stage::Commit));
		let outcome = finish(job, sink, &work_root, &meta, before, staged)?;

		if matches!(outcome, JobOutcome::Encoded { .. }) {
			for (key, observation) in &learned {
				self.quality.record(key, *observation);
			}
		}
		Ok(outcome)
	}

	async fn reserve_memory(
		&self,
		bytes: u64,
		on_event: &(dyn Fn(JobEvent) + Sync),
		cancel: &CancellationToken,
	) -> Result<Option<MemoryReservation<'_>>, JobError> {
		let Some(gate) = self.memory_gate.as_deref() else {
			return Ok(None);
		};

		if !gate.try_reserve(bytes) {
			on_event(JobEvent::Stage(Stage::WaitingForMemory));
			loop {
				tokio::select! {
					() = tokio::time::sleep(MEMORY_POLL) => {}
					() = cancel.cancelled() => return Err(ErrorKind::Cancelled.into()),
				}
				if gate.try_reserve(bytes) {
					break;
				}
			}
		}
		Ok(Some(MemoryReservation { gate, bytes }))
	}

	fn require(&self, kind: MediaKind) -> Result<(), JobError> {
		let problems = self.tools.problems(kind);
		if problems.is_empty() {
			Ok(())
		} else {
			Err(ErrorKind::Toolchain(problems.join("; ")).into())
		}
	}

	fn image_caps(&self) -> ImageCaps {
		ImageCaps {
			libjxl: self.tools.has_libjxl(),
			libaom: self.tools.has_libaom(),
		}
	}

	async fn produce_image(
		&self,
		cx: &Context<'_>,
		input: &ImageInput,
		workspace: &Workspace,
	) -> Result<Produced, JobError> {
		let image = ImageContext {
			tools: &self.tools,
			source: &cx.job.source,
			source_bytes: cx.source_bytes,
			speed: cx.job.speed,
			overrides: cx.job.overrides,
			min_savings: cx.job.min_savings,
			mode: metadata_mode(cx.job),
			threads: self.image_threads,
			caps: self.image_caps(),
			on_event: cx.on_event,
			cancel: cx.cancel,
			memory: self.quality.as_ref(),
			route: image_route(cx.job.options.format),
			forced: cx.job.options.forced(),
		};
		Ok(match image_job::produce(&image, input, workspace).await? {
			ImageOutcome::Output {
				path,
				bytes,
				warnings,
				learned,
			} => Produced::Output(Staged {
				path,
				bytes,
				warnings,
				learned,
			}),
			ImageOutcome::Kept(reason) => Produced::Kept(reason),
		})
	}

	fn video_caps(&self) -> VideoCaps {
		VideoCaps {
			vp9_alpha: self.tools.vp9_alpha,
			libaom: self.tools.has_libaom(),
		}
	}

	fn video_main<'a>(&self, info: &'a MediaInfo, forced: bool) -> Result<&'a Stream, KeepReason> {
		if forced {
			usable_main(info, &self.video_caps())
		} else {
			precheck(info, &self.video_caps())
		}
	}

	async fn produce_audio(
		&self,
		cx: &Context<'_>,
		info: &MediaInfo,
		plan: &FilePlan,
		from_video: bool,
		workspace: &Workspace,
	) -> Result<Produced, JobError> {
		let output = workspace.file(&format!("out.{}", plan.container.extension));
		let plan = &with_clipped_chapters(plan, info, workspace)?;

		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let cmd = encode::command(&self.tools.ffmpeg, &cx.job.source, &output, plan);
		let on_fraction = |fraction| (cx.on_event)(JobEvent::Progress(fraction));
		let duration_s = plan
			.options
			.window
			.map_or(info.format.duration, |window| Some(window.length_s));
		let source_levels = encode::run(&cmd, plan, duration_s, on_fraction, cx.cancel).await?;

		let decoded_s = source_levels.iter().map(|levels| levels.duration_s).fold(0.0, f64::max);
		let shortfall = match plan.options.window {
			Some(window) => window.shortfall(decoded_s),
			None if from_video => extraction_shortfall(info, decoded_s),
			None => info.duration_shortfall(decoded_s),
		};
		if let Some(reason) = shortfall {
			return Err(ErrorKind::SourceDamaged(reason).into());
		}

		(cx.on_event)(JobEvent::Stage(Stage::Metadata));
		let mut warnings = Vec::new();
		if plan.container.cover == Cover::VorbisPicture
			&& !plan.options.strip
			&& let Some(warning) = copy_cover(&cx.job.source, &output, info)
		{
			(cx.on_event)(JobEvent::Warning(warning.clone()));
			warnings.push(warning);
		}

		let bytes = fs::metadata(&output)
			.map_err(|err| JobError::io(IoSide::Destination(&output), &err))?
			.len();
		if !saves_enough(cx.source_bytes, bytes, cx.job.min_savings) {
			if !cx.job.options.forced() {
				return Ok(Produced::Kept(KeepReason::NotSmaller {
					src_bytes: cx.source_bytes,
					out_bytes: bytes,
				}));
			}

			let warning = kept_as_requested(cx.source_bytes, bytes);
			(cx.on_event)(JobEvent::Warning(warning.clone()));
			warnings.push(warning);
		}

		(cx.on_event)(JobEvent::Stage(Stage::Verify));
		let output_levels = verify::measure_output(&self.tools, &output, plan, cx.cancel).await?;
		verify::check_tracks(plan, &source_levels, &output_levels)
			.map_err(|reason| ErrorKind::EncodeFailed(format!("verification failed: {reason}")))?;

		Ok(Produced::Output(Staged {
			path: output,
			bytes,
			warnings,
			learned: Vec::new(),
		}))
	}

	#[allow(clippy::too_many_lines)]
	async fn produce_video(&self, cx: &Context<'_>, workspace: &Workspace) -> Result<Produced, JobError> {
		let source = cx.job.source.as_path();

		(cx.on_event)(JobEvent::Stage(Stage::Probe));
		let (info, probe_json) = probe::probe_video(&self.tools, source, cx.source_bytes, cx.cancel)
			.await
			.map_err(video_probe_error)?;
		let rules = stream_rules(cx.job);
		let planned = self
			.video_main(&info, cx.job.options.forced())
			.map_err(Refusal::from)
			.and_then(|main| plan_streams(&info, &rules).map(|streams| (main, streams)));
		let (main, streams) = match planned {
			Ok(planned) => planned,
			Err(Refusal::Keep(reason)) => return Ok(Produced::Kept(reason)),
			Err(Refusal::Impossible(reason)) => return Err(ErrorKind::EncodeFailed(reason).into()),
		};

		let declared_s = main.declared_duration().or(info.format.duration);
		let window = trim_window(cx.job, declared_s)?;
		let span_s = window.map_or(declared_s, |window| Some(window.length_s));

		if rules.container == MP4_VIDEO && video_plan::has_alpha(main) {
			return Err(
				ErrorKind::EncodeFailed("transparent video can't be stored in MP4; use MKV or WebM".into()).into(),
			);
		}

		let frames = probe::first_frames(&self.tools, source, main.index, 5, cx.cancel)
			.await
			.map_err(video_probe_error)?;

		(cx.on_event)(JobEvent::Stage(Stage::Queued));
		let _slot = tokio::select! {
			slot = self.video_slots.acquire() => slot.expect("the semaphore is never closed"),
			() = cx.cancel.cancelled() => return Err(ErrorKind::Cancelled.into()),
		};
		let estimate = video_plan::memory_estimate(main.width.unwrap_or(0), main.height.unwrap_or(0));
		let _memory = self.reserve_memory(estimate, cx.on_event, cx.cancel).await?;

		(cx.on_event)(JobEvent::Stage(Stage::Analyze));
		let segments = video_quality::analysis_segments(cx.job.speed);
		let analysis = Box::pin(analyze::analyze(
			&self.tools,
			source,
			main,
			declared_s,
			segments,
			window,
			cx.cancel,
		))
		.await?;
		let mut video = plan_video(&info, main, &frames, &analysis, cx.job.speed);
		video.window = window;
		if window.is_some() {
			video.expected_frames = None;
		}
		if cx.job.options.strip {
			video.streams.truncate(1);
		}
		let mut quality = self.video_quality(cx.job, main, &analysis, &video);
		if cx.job.speed != Speed::Fast && !self.tools.has_vmaf() {
			video
				.warnings
				.push("libvmaf is not available: the video quality was not measured".to_owned());
		}

		let learned_bpp = quality.stats.and_then(|stats| stats.bpp);
		if !cx.job.options.forced()
			&& let Some(reason) = learned_bpp.and_then(|bpp| video_plan::efficient_at(&info, main, bpp))
		{
			return Ok(Produced::Kept(KeepReason::AlreadyEfficient(reason)));
		}

		let inputs = VideoInputs {
			info: &info,
			streams: &streams,
			declared_s: span_s,
			span: window.or(declared_s.map(|length_s| TrimWindow { start_s: 0.0, length_s })),
		};
		let verify =
			match Box::pin(self.choose_crf(cx, workspace, &inputs, &analysis, &mut video, &mut quality)).await? {
				Tuning::Keep(reason) => return Ok(self.kept_video(&quality, reason)),
				Tuning::Encode { verify } => verify,
			};

		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let mut temps = Box::pin(self.encode_stage(cx, workspace, source, &video, &streams, span_s)).await?;
		let learned = if verify {
			match Box::pin(self.check_quality(cx, workspace, &inputs, &mut video, &mut temps, &quality)).await? {
				Checked::Passed(_) if window.is_some() => None,
				Checked::Passed(observation) => Some(observation),
				Checked::Short => {
					self.forget_learned(&quality);
					None
				}
				Checked::Missed(reason) => {
					self.forget_learned(&quality);
					return Ok(Produced::Kept(reason));
				}
			}
		} else {
			None
		};

		let job = VideoJob {
			info: &info,
			probe_json: &probe_json,
			first_frame: frames.first(),
			main,
			video: &video,
			streams: &streams,
			container: rules.container,
		};
		Ok(match Box::pin(self.mux_and_verify(cx, workspace, &job, temps)).await? {
			Produced::Output(staged) => Produced::Output(Staged {
				learned: learned
					.map(|observation| (quality.key, observation))
					.into_iter()
					.collect(),
				..staged
			}),
			Produced::Kept(reason) => self.kept_video(&quality, reason),
		})
	}

	fn kept_video(&self, quality: &VideoQuality, reason: KeepReason) -> Produced {
		if matches!(reason, KeepReason::NotSmaller { .. }) {
			self.forget_learned(quality);
		}
		Produced::Kept(reason)
	}

	/// A group whose learned setting led to an output that wasn't smaller, or that missed its check and its retry,
	/// forgets it, so that its next file searches afresh instead of repeating the same encodes.
	fn forget_learned(&self, quality: &VideoQuality) {
		if quality.stats.is_some() {
			self.quality.forget(&quality.key);
		}
	}

	fn video_quality(&self, job: &Job, main: &Stream, analysis: &Analysis, video: &VideoPlan) -> VideoQuality {
		let target = video_quality::target(job.speed, video.encoder.film_grain(), &job.overrides);
		let key = CacheKey {
			group: Group::Video(VideoGroup::of(main, analysis, video)),
			encoder: self.encoder_id(&video.encoder),
			preset: video.encoder.preset(),
			target: target.mean,
			floor: Some(target.floor),
		};
		let stats = self.quality.lookup(&key);

		VideoQuality {
			strategy: video_quality::strategy(job.speed, stats.as_ref(), video.encoder.crf(), self.tools.has_vmaf()),
			slope: stats.and_then(|stats| stats.slope).unwrap_or(DEFAULT_SLOPE),
			key,
			stats,
			target,
		}
	}

	fn encoder_id(&self, encoder: &Encoder) -> String {
		self.tools.encoder_id(match encoder {
			Encoder::SvtAv1 { .. } => SVT_AV1,
			Encoder::Aom { .. } => LIBAOM,
			Encoder::Vp9Alpha { .. } => LIBVPX_VP9,
		})
	}

	fn scorer<'a>(&'a self, cx: &'a Context<'_>, workspace: &'a Workspace, video: &'a VideoPlan) -> Scorer<'a> {
		Scorer {
			tools: &self.tools,
			source: &cx.job.source,
			workspace,
			plan: video,
			threads: self.metric_threads,
			cancel: cx.cancel,
		}
	}

	async fn choose_crf(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		inputs: &VideoInputs<'_>,
		analysis: &Analysis,
		video: &mut VideoPlan,
		quality: &mut VideoQuality,
	) -> Result<Tuning, JobError> {
		let samples = video_quality::search_samples(analysis, inputs.span, cx.job.speed);
		let table_crf = video.encoder.crf();
		let learned = matches!(quality.strategy, Strategy::Fixed { verify: true, .. });
		let verify = match quality.strategy {
			Strategy::Search { .. } if !samples.is_empty() => {
				return Box::pin(self.search_crf(cx, workspace, inputs, &samples, video, quality)).await;
			}
			Strategy::Search { start } => {
				video.warnings.push(unsearched_warning(inputs.declared_s, start));
				quality.strategy = Strategy::Fixed {
					crf: start,
					verify: true,
				};
				true
			}
			Strategy::Fixed { verify, .. } => verify,
		};

		video.encoder = video.encoder.with_crf(quality.start());
		if cx.job.options.forced() {
			return Ok(Tuning::Encode { verify });
		}

		let gate = Box::pin(self.sample_gate(cx, workspace, inputs.info, video, inputs.streams, inputs.declared_s));
		match gate.await? {
			None => Ok(Tuning::Encode { verify }),
			Some(_) if learned && !samples.is_empty() => {
				quality.strategy = Strategy::Search {
					start: quality.start().max(table_crf),
				};
				Box::pin(self.search_crf(cx, workspace, inputs, &samples, video, quality)).await
			}
			Some(reason) => Ok(Tuning::Keep(reason)),
		}
	}

	async fn search_crf(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		inputs: &VideoInputs<'_>,
		samples: &[Sample],
		video: &mut VideoPlan,
		quality: &mut VideoQuality,
	) -> Result<Tuning, JobError> {
		(cx.on_event)(JobEvent::Stage(Stage::Sample));
		let searched = {
			let scorer = self.scorer(cx, workspace, video);
			let on_round = |round: usize| {
				#[allow(clippy::cast_precision_loss)]
				(cx.on_event)(JobEvent::Progress(round as f64 / MAX_PROBES as f64));
			};
			Box::pin(video_quality::search(
				&scorer,
				samples,
				quality.start(),
				quality.target,
				quality.slope,
				on_round,
			))
			.await?
		};

		let found = match searched {
			Searched::Found(found) => found,
			Searched::Unreachable { crf, score } if cx.job.options.forced() => {
				video.encoder = video.encoder.with_crf(crf);
				video.warnings.push(format!(
					"{}; encoded at CRF {crf} without a full check because it was requested",
					video_quality::short_text(crf, score, quality.target)
				));
				return Ok(Tuning::Encode { verify: false });
			}
			Searched::Unreachable { crf, score } => {
				return Ok(Tuning::Keep(video_quality::unreachable_reason(
					crf,
					score,
					quality.target,
				)));
			}
		};
		video.encoder = video.encoder.with_crf(found.crf);
		quality.slope = found.slope;

		if !cx.job.options.forced()
			&& let Some(duration_s) = inputs
				.declared_s
				.filter(|duration| *duration >= video_encode::MIN_PREDICTED_DURATION_S)
		{
			let other = video_encode::other_bitrate(inputs.info, inputs.streams);
			let predicted = video_encode::predicted_bytes(found.sample_bytes, samples, duration_s, other);
			if let Some(reason) = video_encode::predicted_keep(cx.source_bytes, predicted, cx.job.min_savings) {
				return Ok(Tuning::Keep(reason));
			}
		}
		Ok(Tuning::Encode { verify: true })
	}

	async fn check_quality(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		inputs: &VideoInputs<'_>,
		video: &mut VideoPlan,
		temps: &mut EncodedTemps,
		quality: &VideoQuality,
	) -> Result<Checked, JobError> {
		let windows = video_quality::verify_windows(temps.encoded.frames, temps.encoded.decoded_s, cx.job.speed);
		let bounds = video_quality::bounds(&video.encoder);
		let mut attempt = 1;

		loop {
			(cx.on_event)(JobEvent::Stage(Stage::Verify));
			let score = {
				let scorer = self.scorer(cx, workspace, video);
				let on_fraction = |fraction| (cx.on_event)(JobEvent::Progress(fraction));
				Box::pin(scorer.verify(&temps.video.path, &windows, temps.encoded.frames, on_fraction)).await?
			};

			let crf = video.encoder.crf();
			if quality.target.met(score) {
				return Ok(Checked::Passed(Observation {
					setting: search::implied_crf(crf, score, quality.target, quality.slope, bounds),
					slope: Some(quality.slope),
					bpp: output_bpp(&temps.video.path, video, temps.encoded.frames),
				}));
			}

			let lower = if attempt <= MAX_RETRIES {
				search::retry_crf(crf, score, quality.target, quality.slope, bounds)
			} else {
				None
			};
			let Some(lower) = lower else {
				if cx.job.options.forced() {
					temps.warnings.push(format!(
						"{}; it was written because it was requested",
						video_quality::short_text(crf, score, quality.target)
					));
					return Ok(Checked::Short);
				}

				return Ok(Checked::Missed(video_quality::unreachable_reason(
					crf,
					score,
					quality.target,
				)));
			};

			temps.warnings.push(format!(
				"the quality check measured VMAF {:.1} / {:.1} at CRF {crf}; the video was encoded again at CRF {lower}",
				score.mean, score.p5
			));
			video.encoder = video.encoder.with_crf(lower);
			attempt += 1;

			(cx.on_event)(JobEvent::Stage(Stage::Encode));
			Box::pin(self.reencode_video(cx, workspace, inputs.declared_s, video, temps, attempt)).await?;
		}
	}

	async fn reencode_video(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		declared_s: Option<f64>,
		video: &VideoPlan,
		temps: &mut EncodedTemps,
		attempt: u32,
	) -> Result<(), JobError> {
		let _ = fs::remove_file(&temps.video.path);

		let path = workspace.file(&format!("video.a{attempt}.mkv"));
		let cmd = video_encode::video_command(&self.tools.ffmpeg, &cx.job.source, &path, video);
		let on_fraction = |fraction| (cx.on_event)(JobEvent::Progress(fraction));
		let encoded = Box::pin(video_encode::encode_video(
			&cmd,
			video,
			declared_s,
			on_fraction,
			cx.cancel,
		))
		.await?;

		let (temp, _) = self.video_temp(path, video, &encoded, declared_s, cx.cancel).await?;
		temps.video = temp;
		temps.encoded = encoded;
		Ok(())
	}

	async fn video_temp(
		&self,
		path: PathBuf,
		video: &VideoPlan,
		encoded: &EncodedVideo,
		declared_s: Option<f64>,
		cancel: &CancellationToken,
	) -> Result<(Temp, Option<String>), JobError> {
		let info = probe_temp(&self.tools, &path, cancel).await?;
		let start_s = earliest_video_start(&info).unwrap_or(0.0);
		let warning = video_encode::check_video_temp(video, encoded, declared_s, start_s)?;
		Ok((Temp { path, info }, warning))
	}

	async fn sample_gate(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		info: &MediaInfo,
		video: &VideoPlan,
		streams: &StreamPlan,
		declared_s: Option<f64>,
	) -> Result<Option<KeepReason>, JobError> {
		let samples = video_encode::samples(declared_s);
		let Some(duration_s) = declared_s.filter(|_| !samples.is_empty()) else {
			return Ok(None);
		};

		(cx.on_event)(JobEvent::Stage(Stage::Sample));
		let sample_bytes = Box::pin(video_encode::encode_samples(
			&self.tools.ffmpeg,
			&cx.job.source,
			workspace,
			video,
			&samples,
			cx.cancel,
		))
		.await?;

		let other_bits_per_s = video_encode::other_bitrate(info, streams);
		let predicted = video_encode::predicted_bytes(sample_bytes, &samples, duration_s, other_bits_per_s);
		Ok(video_encode::predicted_keep(
			cx.source_bytes,
			predicted,
			cx.job.min_savings,
		))
	}

	#[allow(clippy::too_many_lines)]
	async fn mux_and_verify(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		job: &VideoJob<'_>,
		temps: EncodedTemps,
	) -> Result<Produced, JobError> {
		let source = cx.job.source.as_path();
		let mut warnings: Vec<String> = job
			.video
			.warnings
			.iter()
			.chain(&job.streams.warnings)
			.cloned()
			.collect();
		warnings.extend(temps.warnings);

		(cx.on_event)(JobEvent::Stage(Stage::Mux));
		let container = job.container;
		let keeps_files = container == MATROSKA_VIDEO;
		let strip = cx.job.options.strip;
		if cx.job.keep_metadata && !keeps_files {
			warnings.push(format!(
				"the source metadata attachment was left out: .{} can't hold attachments",
				container.extension
			));
		}

		let metadata = (cx.job.keep_metadata && keeps_files && !strip).then_some((job.probe_json, job.first_frame));
		let attachments = collect_attachments(workspace, &job.streams.extracted, metadata, &mut warnings)?;

		let chapter_file = workspace.file(CHAPTER_FILE);
		let (chapter_source, chapter_count) = match (strip, job.video.window) {
			(true, _) => (ChapterSource::Dropped, 0),
			(false, None) => (ChapterSource::Source, job.info.chapters.len()),
			(false, Some(window)) => {
				let clipped = chapters::clip(&job.info.chapters, window);
				if clipped.is_empty() {
					(ChapterSource::Dropped, 0)
				} else {
					write_chapter_file(&chapter_file, &clipped)?;
					(ChapterSource::File, clipped.len())
				}
			}
		};

		let output = workspace.file(&format!("mux.{}", container.extension));
		let cmd = mux::command(
			&self.tools.ffmpeg,
			&MuxInputs {
				video: &temps.video.path,
				audio: temps.audio.as_ref().map(|temp| temp.path.as_path()),
				source,
				source_bytes: cx.source_bytes,
				attachments: &attachments,
				chapters: (chapter_source == ChapterSource::File).then_some(chapter_file.as_path()),
			},
			&MuxPlan {
				info: job.info,
				video: job.video,
				streams: job.streams,
				keep_metadata: cx.job.keep_metadata,
				container,
				chapters: chapter_source,
				window: job.video.window,
			},
			&output,
		);
		mux::mux(&cmd, cx.cancel).await?;

		let bytes = fs::metadata(&output)
			.map_err(|err| JobError::io(IoSide::Destination(&output), &err))?
			.len();
		if !saves_enough(cx.source_bytes, bytes, cx.job.min_savings) {
			if !cx.job.options.forced() {
				return Ok(Produced::Kept(KeepReason::NotSmaller {
					src_bytes: cx.source_bytes,
					out_bytes: bytes,
				}));
			}

			warnings.push(kept_as_requested(cx.source_bytes, bytes));
		}

		(cx.on_event)(JobEvent::Stage(Stage::Verify));
		let timelines = Timelines {
			source: job.info,
			video_temp: &temps.video.info,
			audio_temp: temps.audio.as_ref().map(|temp| &temp.info),
		};
		let outputs = mux::output_streams(job.video, job.streams);
		let mut offsets = start_offsets(&outputs, &timelines);
		if job.video.window.is_some() {
			for (offset, stream) in offsets.iter_mut().zip(&outputs) {
				if !stream.is_encoded() {
					*offset = None;
				}
			}
		}

		let expected = Expected {
			video: job.video,
			streams: job.streams,
			source_chapters: chapter_count,
			attached_files: attachments.len(),
			start_offsets: offsets,
			source_fps: job.main.fps(),
		};
		verify_output(&self.tools, &output, &expected, cx.cancel).await?;
		if job.video.alpha {
			check_alpha(alpha_ssim(&self.tools, &output, source, job.video, cx.cancel).await?)?;
		}

		for warning in &warnings {
			(cx.on_event)(JobEvent::Warning(warning.clone()));
		}
		Ok(Produced::Output(Staged {
			path: output,
			bytes,
			warnings,
			learned: Vec::new(),
		}))
	}

	// Split out of `produce_video`: the video temp, the audio temp and the extraction run concurrently, and a
	// failed video encode drops (and so kills) the still-running audio encode through `KillOnDrop`.
	async fn encode_stage(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		source: &Path,
		video: &VideoPlan,
		streams: &StreamPlan,
		declared_s: Option<f64>,
	) -> Result<EncodedTemps, JobError> {
		let video_path = workspace.file("video.a1.mkv");
		let audio_temp = workspace.file("audio.mka");
		let audio_plan = video_encode::audio_plan(streams, video.window);
		let video_cmd = video_encode::video_command(&self.tools.ffmpeg, source, &video_path, video);
		let on_fraction = |fraction| (cx.on_event)(JobEvent::Progress(fraction));

		let audio_job = async {
			let Some(plan) = &audio_plan else {
				return Ok(None);
			};
			let cmd = encode::command(&self.tools.ffmpeg, source, &audio_temp, plan);
			encode::run(&cmd, plan, None, |_| {}, cx.cancel).await.map(Some)
		};

		let (encoded, source_levels, ()) = tokio::try_join!(
			Box::pin(video_encode::encode_video(
				&video_cmd,
				video,
				declared_s,
				on_fraction,
				cx.cancel
			)),
			audio_job,
			video_encode::extract(&self.tools.ffmpeg, source, workspace, &streams.extracted, cx.cancel),
		)?;

		let (video_temp, warning) = self
			.video_temp(video_path, video, &encoded, declared_s, cx.cancel)
			.await?;

		let audio = match (&audio_plan, &source_levels) {
			(Some(plan), Some(source_levels)) => {
				self.verify_audio_temp(plan, source_levels, &audio_temp, cx.cancel)
					.await?;
				Some(Temp {
					info: probe_temp(&self.tools, &audio_temp, cx.cancel).await?,
					path: audio_temp,
				})
			}
			_ => None,
		};

		Ok(EncodedTemps {
			video: video_temp,
			audio,
			encoded,
			warnings: warning.into_iter().collect(),
		})
	}

	async fn verify_audio_temp(
		&self,
		plan: &FilePlan,
		source_levels: &[verify::Levels],
		audio_temp: &Path,
		cancel: &CancellationToken,
	) -> Result<(), JobError> {
		let output_levels = verify::measure_output(&self.tools, audio_temp, plan, cancel).await?;
		verify::check_tracks(plan, source_levels, &output_levels)
			.map_err(|reason| ErrorKind::EncodeFailed(format!("verification failed: {reason}")).into())
	}
}

impl Encode for Engine {
	fn encode<'a>(
		&'a self,
		job: &'a Job,
		sink: &'a dyn OutputSink,
		on_event: &'a (dyn Fn(JobEvent) + Sync),
		cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = JobOutcome> + Send + 'a>> {
		Box::pin(self.run(job, sink, on_event, cancel))
	}
}

fn metadata_mode(job: &Job) -> MetadataMode {
	if job.keep_metadata {
		MetadataMode::Full
	} else {
		MetadataMode::IccOnly
	}
}

fn check_damage(source: &Path) -> Result<(), JobError> {
	let damage = integrity::find_damage(source).map_err(|err| JobError::io(IoSide::Source(source), &err))?;
	match damage {
		Some(reason) => Err(ErrorKind::SourceDamaged(reason).into()),
		None => Ok(()),
	}
}

fn media_kind(class: &MediaClass) -> Option<MediaKind> {
	match class {
		MediaClass::Audio(_) => Some(MediaKind::Audio),
		MediaClass::Video(_) => Some(MediaKind::Video),
		MediaClass::StillImage(_) | MediaClass::AnimatedImage(_) => Some(MediaKind::Image),
		MediaClass::Kept(_) | MediaClass::Damaged(_) | MediaClass::Other(_) => None,
	}
}

fn video_container(job: &Job) -> Container {
	job.options
		.format
		.and_then(OutputFormat::video_container)
		.unwrap_or(MATROSKA_VIDEO)
}

fn stream_rules(job: &Job) -> StreamRules {
	StreamRules {
		container: video_container(job),
		overrides: job.overrides,
		strip: job.options.strip,
		trim: job.options.trim.is_some(),
		..StreamRules::new(job.speed, job.keep_metadata)
	}
}

fn with_clipped_chapters(plan: &FilePlan, info: &MediaInfo, workspace: &Workspace) -> Result<FilePlan, JobError> {
	let mut plan = plan.clone();
	if let Some(window) = plan.options.window.filter(|_| !plan.options.strip) {
		let path = workspace.file(CHAPTER_FILE);
		write_chapter_file(&path, &chapters::clip(&info.chapters, window))?;
		plan.options.chapters = Some(path);
	}
	Ok(plan)
}

fn write_chapter_file(path: &Path, clipped: &[chapters::Chapter]) -> Result<(), JobError> {
	fs::write(path, chapters::ffmetadata(clipped)).map_err(|err| JobError::io(IoSide::Destination(path), &err))
}

fn trim_window(job: &Job, duration_s: Option<f64>) -> Result<Option<TrimWindow>, JobError> {
	job.options
		.trim
		.map(|trim| trim.window(duration_s))
		.transpose()
		.map_err(|reason| ErrorKind::EncodeFailed(reason).into())
}

fn image_route(format: Option<OutputFormat>) -> Option<ImageRoute> {
	match format? {
		OutputFormat::Avif => Some(ImageRoute::Avif),
		OutputFormat::Jxl => Some(ImageRoute::Jxl),
		OutputFormat::Mkv
		| OutputFormat::Webm
		| OutputFormat::Mp4
		| OutputFormat::Opus
		| OutputFormat::Ogg
		| OutputFormat::Mka => None,
	}
}

fn extracted(plan: FilePlan) -> FilePlan {
	let cover = match plan.container.cover {
		Cover::Stream => Cover::Attachment,
		other => other,
	};
	FilePlan {
		container: Container {
			cover,
			..plan.container
		},
		..plan
	}
}

fn extraction_refusal(reason: &KeepReason) -> String {
	match reason {
		KeepReason::Unsupported(why) => format!("the audio can't be extracted: {why}"),
		_ => "the audio can't be extracted".to_owned(),
	}
}

fn extraction_shortfall(info: &MediaInfo, decoded_s: f64) -> Option<String> {
	let declared = info
		.audio_streams()
		.filter_map(Stream::declared_duration)
		.fold(0.0, f64::max);

	(declared > 0.0 && declared - decoded_s > (declared * 0.02).max(1.0))
		.then(|| format!("decodes to {decoded_s:.1} s but its audio declares {declared:.1} s"))
}

fn copy_cover(source: &Path, output: &Path, info: &MediaInfo) -> Option<String> {
	let pictures = cover::source_pictures(source);
	if pictures.is_empty() && info.streams.iter().any(Stream::is_attached_pic) {
		return Some("cover art not copied: covers in this format can't be read yet".to_owned());
	}

	cover::add_pictures(output, pictures)
		.err()
		.map(|err| format!("cover art not copied: {err}"))
}

fn collect_attachments(
	workspace: &Workspace,
	extracted: &[Extracted],
	metadata: Option<(&[u8], Option<&probe::Frame>)>,
	warnings: &mut Vec<String>,
) -> Result<Vec<Attachment>, JobError> {
	let mut attachments = Vec::new();
	for stream in extracted {
		let path = workspace.file(&stream.work_file());
		if fs::metadata(&path).is_ok_and(|meta| meta.len() > 0) {
			attachments.push(Attachment {
				path,
				file_name: stream.file_name.clone(),
				mime_type: stream.mime_type.clone(),
			});
		} else {
			warnings.push(format!("{} not attached: the stream is empty", stream.file_name));
		}
	}

	if let Some((probe_json, first_frame)) = metadata {
		attachments.push(write_source_metadata(workspace, probe_json, first_frame)?);
	}
	Ok(attachments)
}

fn write_source_metadata(
	workspace: &Workspace,
	probe_json: &[u8],
	first_frame: Option<&probe::Frame>,
) -> Result<Attachment, JobError> {
	let ffprobe: serde_json::Value = serde_json::from_slice(probe_json).unwrap_or_default();
	let side_data = first_frame.map(|frame| &frame.side_data_list);
	let document = serde_json::json!({ "ffprobe": ffprobe, "frame_side_data": side_data });

	let path = workspace.file(SOURCE_METADATA);
	fs::write(&path, document.to_string()).map_err(|err| JobError::io(IoSide::Destination(&path), &err))?;
	Ok(Attachment {
		path,
		file_name: SOURCE_METADATA.to_owned(),
		mime_type: "application/json".to_owned(),
	})
}

fn video_probe_error(err: ProbeError) -> JobError {
	match err {
		ProbeError::Run(err) => err.into(),
		ProbeError::Failed(log) => JobError::with_log(
			ErrorKind::SourceDamaged("ffprobe could not read the whole file".into()),
			log,
		),
		ProbeError::Json(err) => ErrorKind::Toolchain(err.to_string()).into(),
	}
}

fn disk_full(err: JobError, work_root: &Path, source_bytes: u64) -> JobError {
	if !err.is_disk_full() {
		return err;
	}

	let need = source_bytes.saturating_add(source_bytes / 10);
	let free = free_space(work_root).unwrap_or(0);
	JobError::with_log(ErrorKind::InsufficientSpace { need, free }, err.log_tail)
}

fn open_workspace(
	sink: &dyn OutputSink,
	work_root: &Path,
	source: &Path,
	source_bytes: u64,
) -> Result<Workspace, JobError> {
	check_space(work_root, source_bytes)?;
	Workspace::for_sink(sink, source).map_err(|err| JobError::io(IoSide::Destination(work_root), &err))
}

fn finish(
	job: &Job,
	sink: &dyn OutputSink,
	work_root: &Path,
	meta: &Metadata,
	before: SourceTimes,
	staged: Staged,
) -> Result<JobOutcome, JobError> {
	if source_changed(&job.source, meta.len(), &before) {
		return Err(ErrorKind::SourceChanged.into());
	}

	let artifact = Artifact {
		path: staged.path,
		source: job.source.clone(),
		times: job.keep_metadata.then_some(before),
	};
	match sink.commit(&artifact) {
		Ok(output) => Ok(JobOutcome::Encoded {
			output,
			src_bytes: meta.len(),
			out_bytes: staged.bytes,
			warnings: staged.warnings,
		}),
		Err(CommitError::Exists(path)) => Ok(skipped(&format!("output already exists: {}", display_path(&path)))),
		Err(CommitError::IsSource(path)) => Ok(skipped(&format!("{} is the source file", display_path(&path)))),
		Err(CommitError::Io(err)) => Err(JobError::io(
			IoSide::Destination(work_root.parent().unwrap_or(work_root)),
			&err,
		)),
	}
}

fn unsearched_warning(declared_s: Option<f64>, crf: f64) -> String {
	let why = if declared_s.is_some() {
		"no part of the video could be sampled"
	} else {
		"the video's duration is unknown"
	};
	format!("the CRF was not searched because {why}; the video was encoded at CRF {crf} and checked")
}

#[allow(clippy::cast_precision_loss)]
fn output_bpp(path: &Path, video: &VideoPlan, frames: u64) -> Option<f64> {
	let bytes = fs::metadata(path).ok()?.len();
	let pixels = f64::from(video.width) * f64::from(video.height) * frames as f64;
	(pixels > 0.0).then(|| bytes as f64 * 8.0 / pixels)
}

#[allow(clippy::cast_precision_loss)]
fn saves_enough(src_bytes: u64, out_bytes: u64, min_savings: f64) -> bool {
	(out_bytes as f64) <= (src_bytes as f64) * (1.0 - min_savings)
}

fn source_changed(source: &Path, size: u64, before: &SourceTimes) -> bool {
	let unchanged =
		fs::metadata(source).is_ok_and(|meta| meta.len() == size && meta.modified().ok() == Some(before.modified));
	!unchanged
}

fn skipped(reason: &str) -> JobOutcome {
	JobOutcome::Skipped {
		reason: reason.to_owned(),
	}
}
