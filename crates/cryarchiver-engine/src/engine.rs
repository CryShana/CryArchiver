use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;

use crate::audio::plan::{
	AudioPlan, AudioRules, FilePlan, TrackJob, TrackPlan, merge_file, mergeable, mixed_languages_warning,
	plan_file_with, retarget,
};
use crate::audio::{cover, encode, tags, verify};
use crate::commit::{Artifact, CommitError, OutputSink, SourceTimes, check_space, free_space};
use crate::container::{Container, Cover, MATROSKA_VIDEO, MP4_VIDEO};
use crate::detect::{self, Detection, MediaClass};
use crate::format::{OutputFormat, check_bitrate_cap, check_kind};
use crate::images::animated::{self, Animation};
use crate::images::job::{self as image_job, ImageCaps, ImageContext, ImageInput, ImageOutcome};
use crate::images::plan::ImageRoute;
use crate::integrity;
use crate::metadata::MetadataMode;
use crate::options::{
	JobOptions, Resize, TrimWindow, VideoEncoderChoice, kept_as_requested, kept_as_requested_part, upscaled,
};
use crate::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason, Refusal};
use crate::preset::{Overrides, Speed};
use crate::probe::{self, Frame, MediaInfo, ProbeError, Stream};
use crate::quality::keys::{CacheKey, Group, VideoGroup};
use crate::quality::memory::{GroupStats, Observation, Offset, QualityMemory, SharedMemory};
use crate::quality::search::{self, DEFAULT_SLOPE, MAX_PROBES, Target};
use crate::quality::vmaf::Score;
use crate::text::{display_path, fold_repeats};
use crate::toolchain::{LIBAOM, LIBVPX_VP9, MediaKind, SVT_AV1, Toolchain};
use crate::video::analyze::{Analysis, ScanType};
use crate::video::burn::{self, Burn};
use crate::video::colour::FrameFormat;
use crate::video::encode::{EncodedVideo, Sample};
use crate::video::mux::{self, Attachment, ChapterSource, CueFile, MuxInputs, MuxPlan};
use crate::video::plan::{
	self as video_plan, Encoder, VideoCaps, VideoPlan, plan_video, precheck, threads_for, usable_main,
};
use crate::video::quality::{self as video_quality, Aim, GrainChoice, MAX_RETRIES, Scorer, Searched, Strategy};
use crate::video::streams::{CueFormat, Extracted, StreamPlan, StreamRules, plan_streams};
use crate::video::verify::{
	Expected, FrameTiming, Retiming, Timelines, alpha_ssim, check_alpha, check_frame_times, check_sync,
	earliest_video_start, probe_temp, start_offsets, sync_tolerance_s, synced_streams, undecoded_lead, verify_output,
};
use crate::video::{VideoStarts, analyze, chapters, colour, cues, discontinuous, encode as video_encode, titles};
use crate::workspace::Workspace;

const SOURCE_METADATA: &str = "source-metadata.json";
const CHAPTER_FILE: &str = "chapters.txt";
const MEMORY_POLL: Duration = Duration::from_millis(250);
const ANIMATION_FRAMES: &str = "frames.mkv";
const SQUARE_PIXEL_SLACK: f64 = 1e-3;
const AUDIO_DROPPED: &str = "an audio track was dropped: animated AVIF has no sound";
const SUBTITLES_DROPPED: &str =
	"a subtitle track was dropped: animated AVIF can't hold subtitles, but --burn-subs draws one into the picture";
const FRAMES_SPACE_HINT: &str = "an animated AVIF is made from lossless frames stored in the work folder: use \
                                 --ss/--to for a shorter clip or --resize for smaller frames";

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
	video_threads: Option<usize>,
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
	/// Under a trim, the window's share of the source: the size the output is weighed and reported against.
	source_share: Option<u64>,
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
	Video(StreamRules),
	/// A video written as an animated AVIF, with the subtitle track to burn in, if any.
	Animation(Option<Burn>),
	Image(Box<ImageInput>),
}

struct VideoJob<'a> {
	info: &'a MediaInfo,
	probe_json: &'a [u8],
	first_frame: Option<&'a Frame>,
	main: &'a Stream,
	scan: ScanType,
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
	/// The source's size, or under a trim the window's share of it: what a predicted output is weighed against.
	compared_bytes: u64,
}

struct AnimationJob<'a> {
	video: &'a VideoPlan,
	span_s: Option<f64>,
	/// The window's share of the source, which the AVIF is weighed against.
	share: Option<u64>,
	/// The space the frames and the AVIF can take, when the clip's length and rate are known.
	need: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Span {
	declared_s: Option<f64>,
	window: Option<TrimWindow>,
	/// The window's length, or the whole video's.
	length_s: Option<f64>,
}

struct VideoQuality {
	key: CacheKey,
	stats: Option<GroupStats>,
	target: Target,
	strategy: Strategy,
	slope: f64,
	/// The plan's film grain was left out, for its group's learned setting or after a search without it won.
	without_grain: bool,
	/// What the search samples scored at the CRF the video is encoded at.
	predicted: Option<Score>,
}

impl VideoQuality {
	fn start(&self) -> f64 {
		match self.strategy {
			Strategy::Search { start } => start,
			Strategy::Fixed { crf, .. } => crf,
		}
	}

	/// A search toward `nominal` for a full encode of `length_s`, moved by what the group learned about its samples.
	fn aim(&self, nominal: Target, length_s: Option<f64>) -> Aim {
		Aim {
			nominal,
			search: video_quality::search_target(nominal, self.stats.and_then(|stats| stats.offset)),
			length_s,
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
			video_threads: None,
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

	#[must_use]
	pub fn with_threads(self, threads: usize) -> Self {
		let threads = threads.max(1);

		Self {
			image_threads: threads,
			metric_threads: threads,
			video_threads: Some(threads),
			..self
		}
	}

	fn job_threads(&self, job: &Job, fallback: usize) -> usize {
		thread_budget(self.video_threads, job.options.threads, fallback)
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
					forced: job.options.forced(mergeable(&info)),
					reencode: job.options.trim.is_some(),
				};
				match plan_file_with(&info, job.speed, &job.overrides, rules) {
					AudioPlan::Encode(plan) => {
						let plan = if job.options.merge_audio {
							merge_file(plan, &info, job.speed, &job.overrides)
						} else {
							plan
						};
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
						let plan = if job.options.merge_audio {
							merge_file(plan, &info, job.speed, &job.overrides)
						} else {
							plan
						};
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
			MediaClass::Video(info) if job.options.format == Some(OutputFormat::Avif) => {
				self.check_animation(job, OutputFormat::Avif)?;
				check_damage(&job.source)?;

				on_event(JobEvent::Stage(Stage::Plan));
				let burn = burn_pick(job, &info)?;
				self.check_burn_tools(burn, &info)?;
				match self.video_main(&info, job) {
					Ok(_) => Planned::Animation(burn),
					Err(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
				}
			}
			MediaClass::Video(info) => {
				self.require(MediaKind::Video)?;
				check_damage(&job.source)?;

				on_event(JobEvent::Stage(Stage::Plan));
				let rules = stream_rules(job, &info)?;
				self.check_burn_tools(rules.burn, &info)?;
				let planned = self
					.video_main(&info, job)
					.map_err(Refusal::from)
					.and_then(|_| plan_streams(&info, &rules));
				match planned {
					Ok(_) => Planned::Video(rules),
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
				let prepared = image_job::prepare(
					&job.source,
					&info,
					animated,
					job.drop_aux,
					metadata_mode(job),
					job.options.resize,
				)?;
				match prepared {
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
			Planned::Audio { .. } | Planned::Video(_) | Planned::Animation(_) => None,
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
			Planned::Video(rules) => Box::pin(self.produce_video(&cx, &workspace, *rules)).await,
			Planned::Animation(burn) => Box::pin(self.produce_video_animation(&cx, &workspace, *burn)).await,
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
			threads: self.job_threads(cx.job, self.image_threads),
			caps: self.image_caps(),
			on_event: cx.on_event,
			cancel: cx.cancel,
			memory: self.quality.as_ref(),
			route: cx
				.job
				.options
				.advanced
				.image_encoder
				.or(image_route(cx.job.options.format)),
			forced: cx.job.options.forced_image(),
			resize: cx.job.options.resize,
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
				source_share: None,
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

	fn video_main<'a>(&self, info: &'a MediaInfo, job: &Job) -> Result<&'a Stream, KeepReason> {
		if job.options.forced_video(mergeable(info)) {
			usable_main(info, &self.video_caps(), job.options.resize)
		} else {
			precheck(info, &self.video_caps())
		}
	}

	/// A video slot, then the memory for decoding at the source's size and at the output's.
	async fn video_resources(
		&self,
		cx: &Context<'_>,
		main: &Stream,
		size: Option<(u32, u32)>,
	) -> Result<(SemaphorePermit<'_>, Option<MemoryReservation<'_>>), JobError> {
		(cx.on_event)(JobEvent::Stage(Stage::Queued));
		let slot = tokio::select! {
			slot = self.video_slots.acquire() => slot.expect("the semaphore is never closed"),
			() = cx.cancel.cancelled() => return Err(ErrorKind::Cancelled.into()),
		};

		let decoded = video_plan::memory_estimate(main.width.unwrap_or(0), main.height.unwrap_or(0));
		let estimate = size.map_or(decoded, |(width, height)| {
			decoded.max(video_plan::memory_estimate(width, height))
		});
		let memory = self.reserve_memory(estimate, cx.on_event, cx.cancel).await?;
		Ok((slot, memory))
	}

	async fn analyze_main(&self, cx: &Context<'_>, main: &Stream, span: Span) -> Result<Analysis, JobError> {
		(cx.on_event)(JobEvent::Stage(Stage::Analyze));
		let segments = video_quality::analysis_segments(cx.job.speed, span.length_s);
		Box::pin(analyze::analyze(
			&self.tools,
			&cx.job.source,
			main,
			span.declared_s,
			segments,
			span.window,
			cx.cancel,
		))
		.await
	}

	fn budget_threads(&self, job: &Job, video: &mut VideoPlan) {
		let cores = std::thread::available_parallelism().map_or(1, usize::from);
		if self.video_threads.is_some() || job.options.threads.is_some() {
			video.threads = Some(threads_for(self.job_threads(job, cores), cores));
		}
	}

	/// Sets the trim window and the frame count the encode must give: none under a trim, and otherwise the
	/// decodable packets when their count can be trusted.
	async fn plan_window(
		&self,
		cx: &Context<'_>,
		main: &Stream,
		span: Span,
		scan_type: ScanType,
		video: &mut VideoPlan,
	) -> Result<(), JobError> {
		video.window = span.window;
		if span.window.is_some() {
			video.expected_frames = None;
		} else if video.expected_frames.is_some() {
			let undecoded = undecoded_lead(&self.tools, &cx.job.source, main.index, cx.cancel).await?;
			video.expected_frames = video_plan::decodable_frames(main, span.declared_s, scan_type, undecoded);
		}
		Ok(())
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
		let mut plan = with_clipped_chapters(plan, info, workspace)?;
		plan.options.titles = encoded_titles(info, &plan.tracks);
		let plan = &plan;

		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let cmd = encode::command(&self.tools.ffmpeg, &cx.job.source, &output, plan);
		let on_fraction = |fraction| (cx.on_event)(JobEvent::Progress(fraction));
		let duration_s = plan
			.options
			.window
			.map_or(info.format.duration, |window| Some(window.length_s));
		let source_levels = encode::run(&cmd, plan, duration_s, on_fraction, cx.cancel).await?;

		let decoded_s = source_levels.iter().map(|levels| levels.duration_s).fold(0.0, f64::max);
		let shortfall = if plan.options.merge_from.is_empty() {
			match plan.options.window {
				Some(window) => window.shortfall(decoded_s),
				None if from_video => extraction_shortfall(info, decoded_s),
				None => info.duration_shortfall(decoded_s),
			}
		} else {
			merge_shortfall(info, &plan.options.merge_from, plan.options.window, decoded_s)
		};
		if let Some(reason) = shortfall {
			return Err(ErrorKind::SourceDamaged(reason).into());
		}

		(cx.on_event)(JobEvent::Stage(Stage::Metadata));
		let mut warnings = Vec::new();
		if let Some(warning) = mixed_languages_warning(info, &plan.options.merge_from) {
			(cx.on_event)(JobEvent::Warning(warning.clone()));
			warnings.push(warning);
		}

		if plan.container.cover == Cover::VorbisPicture
			&& !plan.options.strip
			&& let Some(warning) = copy_cover(&cx.job.source, &output, info)
		{
			(cx.on_event)(JobEvent::Warning(warning.clone()));
			warnings.push(warning);
		}

		if plan.container.cover == Cover::VorbisPicture
			&& !plan.options.strip
			&& let Err(err) = tags::rename_keys(&output)
		{
			let warning = format!("music tags keep their ID3 names: {err}");
			(cx.on_event)(JobEvent::Warning(warning.clone()));
			warnings.push(warning);
		}

		let bytes = fs::metadata(&output)
			.map_err(|err| JobError::io(IoSide::Destination(&output), &err))?
			.len();
		let share = source_share(cx.source_bytes, plan.options.window, info.format.duration);
		if !saves_enough(share.unwrap_or(cx.source_bytes), bytes, cx.job.min_savings) {
			if !cx.job.options.forced(mergeable(info)) {
				return Ok(Produced::Kept(KeepReason::NotSmaller {
					src_bytes: share.unwrap_or(cx.source_bytes),
					out_bytes: bytes,
				}));
			}

			let warning = not_smaller_warning(cx.source_bytes, share, bytes);
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
			source_share: share,
			warnings,
			learned: Vec::new(),
		}))
	}

	#[allow(clippy::too_many_lines)]
	async fn produce_video(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		rules: StreamRules,
	) -> Result<Produced, JobError> {
		let source = cx.job.source.as_path();

		(cx.on_event)(JobEvent::Stage(Stage::Probe));
		let (info, probe_json) = probe::probe_video(&self.tools, source, cx.source_bytes, cx.cancel)
			.await
			.map_err(video_probe_error)?;
		let planned = self
			.video_main(&info, cx.job)
			.map_err(Refusal::from)
			.and_then(|main| plan_streams(&info, &rules).map(|streams| (main, streams)));
		let (main, streams) = match planned {
			Ok(planned) => planned,
			Err(Refusal::Keep(reason)) => return Ok(Produced::Kept(reason)),
			Err(Refusal::Impossible(reason)) => return Err(ErrorKind::EncodeFailed(reason).into()),
		};

		let span = video_span(cx.job, &info, main)?;
		let Span {
			declared_s,
			window,
			length_s: span_s,
		} = span;

		let shown = video_plan::shown_size(main);
		let size = cx
			.job
			.options
			.resize
			.map(|resize| resize.size(shown, true))
			.transpose()
			.map_err(ErrorKind::EncodeFailed)?;

		if rules.container == MP4_VIDEO && video_plan::has_alpha(main) {
			return Err(
				ErrorKind::EncodeFailed("transparent video can't be stored in MP4; use MKV or WebM".into()).into(),
			);
		}

		let frames = probe::first_frames(&self.tools, source, main.index, 5, cx.cancel)
			.await
			.map_err(video_probe_error)?;

		let _resources = self.video_resources(cx, main, size).await?;
		let analysis = Box::pin(self.analyze_main(cx, main, span)).await?;
		let mut video = plan_video(&info, main, &frames, &analysis, cx.job.speed, size);
		video.warnings.extend(size.and_then(|size| upscaled(shown, size)));
		self.budget_threads(cx.job, &mut video);

		if let Some(choice) = cx.job.options.advanced.video_encoder {
			if choice == VideoEncoderChoice::Aom && !self.tools.has_libaom() {
				return Err(ErrorKind::EncodeFailed("--video-encoder aom needs ffmpeg with libaom-av1".into()).into());
			}
			video_plan::force_encoder(&mut video, choice, cx.job.speed).map_err(ErrorKind::EncodeFailed)?;
		}

		video.max_bitrate_kbps = cx.job.options.max_bitrate_kbps;

		if video.encoder.film_grain()
			&& main.codec_name == "av1"
			&& probe::av1_film_grain(&self.tools, source, main.index, cx.cancel)
				.await
				.map_err(video_probe_error)?
		{
			video.encoder = video.encoder.without_grain();
		}
		self.plan_window(cx, main, span, analysis.scan, &mut video).await?;
		if cx.job.options.strip {
			video.streams.truncate(1);
		}
		video.starts = VideoStarts::of(&info, &video.streams);
		if let Some(chosen) = rules.burn {
			Box::pin(self.prepare_burn(cx, workspace, &info, &mut video, chosen)).await?;
		}

		let mut quality = self.video_quality(cx.job, main, &analysis, &video);
		Self::apply_learned_grain(cx.job, &mut video, &mut quality);
		if let Some(crf) = cx.job.options.advanced.crf {
			video.warnings.push(format!(
				"--crf fixed the CRF at {}: the video quality was not measured",
				video.encoder.with_crf(crf).crf()
			));
		} else if cx.job.speed != Speed::Fast && !self.tools.has_vmaf() {
			video
				.warnings
				.push("libvmaf is not available: the video quality was not measured".to_owned());
		}

		let learned_bpp = quality.stats.and_then(|stats| stats.bpp);
		if !cx.job.options.forced_video(mergeable(&info))
			&& let Some(reason) = learned_bpp.and_then(|bpp| video_plan::efficient_at(&info, main, bpp))
		{
			return Ok(Produced::Kept(KeepReason::AlreadyEfficient(reason)));
		}

		let inputs = VideoInputs {
			info: &info,
			streams: &streams,
			declared_s: span_s,
			span: window.or(declared_s.map(|length_s| TrimWindow { start_s: 0.0, length_s })),
			compared_bytes: source_share(cx.source_bytes, window, declared_s).unwrap_or(cx.source_bytes),
		};
		let verify =
			match Box::pin(self.choose_crf(cx, workspace, &inputs, &analysis, &mut video, &mut quality)).await? {
				Tuning::Keep(reason) => return Ok(self.kept_video(&quality, reason)),
				Tuning::Encode { verify } => verify,
			};

		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let mut temps = Box::pin(self.encode_stage(cx, workspace, source, &video, &inputs)).await?;
		let learned = if verify {
			match Box::pin(self.check_quality(cx, workspace, &inputs, &mut video, &mut temps, &quality)).await? {
				Checked::Passed(_) if window.is_some() || size.is_some() => None,
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
			scan: analysis.scan,
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

	async fn prepare_burn(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		info: &MediaInfo,
		video: &mut VideoPlan,
		chosen: Burn,
	) -> Result<(), JobError> {
		if video.alpha {
			return Err(ErrorKind::EncodeFailed("--burn-subs can't be used on transparent video".into()).into());
		}

		let start_s = burn::prepare(&self.tools, &cx.job.source, workspace, info, chosen, cx.cancel).await?;

		let size_percent = cx
			.job
			.options
			.burn_subs
			.as_ref()
			.map_or(0, |burn_subs| burn_subs.size_percent);
		match burn::subtitle_filter(chosen.kind, size_percent) {
			Some(filter) => video.subtitles = Some(filter),
			None => video.overlay = Some(burn::overlay(info, video.streams[0], chosen, start_s.unwrap_or(0.0))),
		}
		Ok(())
	}

	fn check_burn_tools(&self, chosen: Option<Burn>, info: &MediaInfo) -> Result<(), JobError> {
		let Some(chosen) = chosen else {
			return Ok(());
		};

		let missing = burn::missing_tools(chosen, info, &self.tools);
		if missing.is_empty() {
			return Ok(());
		}

		Err(ErrorKind::Toolchain(format!(
			"--burn-subs needs ffmpeg's {}, which this ffmpeg doesn't have",
			missing.join(" and ")
		))
		.into())
	}

	fn check_animation(&self, job: &Job, format: OutputFormat) -> Result<(), JobError> {
		if job.options.max_bitrate_kbps.is_some() {
			check_bitrate_cap(format).map_err(ErrorKind::EncodeFailed)?;
		}

		self.require(MediaKind::Image)?;
		if !self.tools.has_libaom() {
			return Err(ErrorKind::Toolchain("an animated AVIF needs ffmpeg with libaom-av1".into()).into());
		}

		Ok(())
	}

	/// The main video, trimmed, deinterlaced, resized and with any subtitles burned in as the video path does it, is
	/// stored as lossless RGB frames, which the animation path encodes and checks the AVIF against.
	async fn produce_video_animation(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		burn: Option<Burn>,
	) -> Result<Produced, JobError> {
		let source = cx.job.source.as_path();

		(cx.on_event)(JobEvent::Stage(Stage::Probe));
		let (info, _) = probe::probe_video(&self.tools, source, cx.source_bytes, cx.cancel)
			.await
			.map_err(video_probe_error)?;
		let main = match self.video_main(&info, cx.job) {
			Ok(main) => main,
			Err(reason) => return Ok(Produced::Kept(reason)),
		};

		let span = video_span(cx.job, &info, main)?;
		check_frame_count(estimated_frames(span, main.fps(), ScanType::Progressive))?;

		let shown = video_plan::shown_size(main);
		let size = animation_size(cx.job.options.resize, main).map_err(ErrorKind::EncodeFailed)?;
		let frames = probe::first_frames(&self.tools, source, main.index, 5, cx.cancel)
			.await
			.map_err(video_probe_error)?;

		let _resources = self.video_resources(cx, main, size).await?;
		let analysis = Box::pin(self.analyze_main(cx, main, span)).await?;
		let frame_count = estimated_frames(span, main.fps(), analysis.scan);
		check_frame_count(frame_count)?;

		let mut video = plan_video(&info, main, &frames, &analysis, cx.job.speed, size);
		let format = FrameFormat::of(main, &video.colour, video.alpha);
		video.filters = colour::rgb_frames(main, &video.colour, format).map_err(ErrorKind::EncodeFailed)?;
		video.warnings = video_plan::animation_warnings(main, &frames);
		video.streams.truncate(1);
		video.starts = VideoStarts::of(&info, &video.streams);
		self.budget_threads(cx.job, &mut video);
		self.plan_window(cx, main, span, analysis.scan, &mut video).await?;

		if let Some(chosen) = burn {
			Box::pin(self.prepare_burn(cx, workspace, &info, &mut video, chosen)).await?;
		}

		let mut warnings = dropped_streams(&info, burn);
		warnings.extend(ignored_flags(cx.job));
		warnings.extend(video.warnings.iter().cloned());
		warnings.extend(size.and_then(|size| upscaled(shown, size)));
		if cx.job.keep_metadata {
			warnings.push("metadata is not carried into animated AVIF yet".to_owned());
		}

		let share = source_share(cx.source_bytes, span.window, span.declared_s);
		let job = AnimationJob {
			video: &video,
			span_s: span.length_s,
			share,
			need: frame_count
				.map(|frames| frames_bytes(frames, &video, format).saturating_add(share.unwrap_or(cx.source_bytes))),
		};
		Box::pin(self.animate(cx, workspace, &job, warnings)).await
	}

	async fn animate(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		job: &AnimationJob<'_>,
		warnings: Vec<String>,
	) -> Result<Produced, JobError> {
		let path = workspace.file(ANIMATION_FRAMES);
		if let Some(need) = job.need {
			let free = free_space(&path).unwrap_or(u64::MAX);
			if free < need {
				return Err(frames_space_error(need, free, Vec::new()));
			}
		}

		Box::pin(self.prepare_and_encode(cx, workspace, job, path, warnings))
			.await
			.map_err(|err| match job.need {
				Some(need) if err.is_disk_full() => {
					let free = free_space(&workspace.file(ANIMATION_FRAMES)).unwrap_or(0);
					frames_space_error(need, free, err.log_tail)
				}
				_ => err,
			})
	}

	/// The frames are the first half of the progress and the AVIF the second.
	async fn prepare_and_encode(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		job: &AnimationJob<'_>,
		path: PathBuf,
		mut warnings: Vec<String>,
	) -> Result<Produced, JobError> {
		let video = job.video;
		let span_s = job.span_s;

		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let cmd = video_encode::frames_command(&self.tools.ffmpeg, &cx.job.source, &path, video);
		let on_fraction = |fraction: f64| (cx.on_event)(JobEvent::Progress(fraction / 2.0));
		let encoded = Box::pin(video_encode::encode_video(&cmd, video, span_s, on_fraction, cx.cancel)).await?;
		let (frames, warning) = self.video_temp(path, video, &encoded, span_s, cx.cancel).await?;
		warnings.extend(warning);

		let prepared = frames.info.streams.first();
		let size = (
			prepared.and_then(|stream| stream.width).unwrap_or(video.width),
			prepared.and_then(|stream| stream.height).unwrap_or(video.height),
		);
		let timeline = animated::read_video_timeline(&self.tools, &frames.path, size, cx.cancel).await?;
		if let Some(reason) = animated::too_many_frames(u64::from(timeline.frames)) {
			return Err(ErrorKind::EncodeFailed(reason).into());
		}

		if timeline.frames < 2 {
			return Err(ErrorKind::EncodeFailed(
				"the clip has a single frame, too few for an animation; use a longer range".into(),
			)
			.into());
		}

		let animation = Animation {
			alpha: video.alpha && animated::has_alpha(&self.tools, &frames.path, cx.cancel).await?,
			timeline,
			size: None,
		};
		let second_half = |event: JobEvent| match event {
			JobEvent::Progress(fraction) => (cx.on_event)(JobEvent::Progress(0.5 + fraction / 2.0)),
			JobEvent::Stage(Stage::Encode) => {}
			other => (cx.on_event)(other),
		};
		let image = ImageContext {
			tools: &self.tools,
			source: &frames.path,
			source_bytes: job.share.unwrap_or(cx.source_bytes),
			speed: cx.job.speed,
			overrides: cx.job.overrides,
			min_savings: cx.job.min_savings,
			mode: metadata_mode(cx.job),
			threads: self.job_threads(cx.job, self.image_threads),
			caps: self.image_caps(),
			on_event: &second_half,
			cancel: cx.cancel,
			memory: self.quality.as_ref(),
			route: Some(ImageRoute::Avif),
			forced: true,
			resize: None,
		};
		let encoded = image_job::encode_animation(&image, &frames.path, &animation, workspace, fold_repeats(warnings));
		Ok(match Box::pin(encoded).await? {
			ImageOutcome::Output {
				path, bytes, warnings, ..
			} => Produced::Output(Staged {
				path,
				bytes,
				source_share: job.share,
				warnings,
				learned: Vec::new(),
			}),
			ImageOutcome::Kept(reason) => Produced::Kept(reason),
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
			encoder: match video.max_bitrate_kbps {
				Some(kbps) => format!("{}+mbr{kbps}", self.encoder_id(&video.encoder)),
				None => self.encoder_id(&video.encoder),
			},
			preset: video.encoder.preset(),
			target: target.mean,
			floor: Some(target.floor),
		};
		let (stats, strategy) = if let Some(crf) = job.options.advanced.crf {
			(None, Strategy::Fixed { crf, verify: false })
		} else {
			let stats = self.quality.lookup(&key);
			let strategy =
				video_quality::strategy(job.speed, stats.as_ref(), video.encoder.crf(), self.tools.has_vmaf());
			(stats, strategy)
		};

		VideoQuality {
			strategy,
			slope: stats.and_then(|stats| stats.slope).unwrap_or(DEFAULT_SLOPE),
			key,
			stats,
			target,
			without_grain: false,
			predicted: None,
		}
	}

	/// A group that learned it comes out smaller without film grain is encoded without it, against the full target.
	fn apply_learned_grain(job: &Job, video: &mut VideoPlan, quality: &mut VideoQuality) {
		if quality.stats.is_some_and(|stats| stats.without_grain) && video.encoder.film_grain() {
			video.encoder = video.encoder.without_grain();
			quality.target = video_quality::target(job.speed, false, &job.overrides);
			quality.without_grain = true;
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
			threads: self.job_threads(cx.job, self.metric_threads),
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
		if cx.job.options.forced_video(mergeable(inputs.info)) {
			return Ok(Tuning::Encode { verify });
		}

		let gate = Box::pin(self.sample_gate(cx, workspace, inputs, video));
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
		let level = video.encoder.grain_level();
		let passes = if level.is_some() { 2 } else { 1 };
		let progress = |pass: usize| {
			move |round: usize| {
				#[allow(clippy::cast_precision_loss)]
				(cx.on_event)(JobEvent::Progress(
					(pass * MAX_PROBES + round) as f64 / (passes * MAX_PROBES) as f64,
				));
			}
		};
		let mut searched = {
			let scorer = self.scorer(cx, workspace, video);
			let aim = quality.aim(quality.target, inputs.declared_s);
			Box::pin(video_quality::search(
				&scorer,
				samples,
				quality.start(),
				aim,
				quality.slope,
				progress(0),
			))
			.await?
		};

		if let Some(level) = level {
			let plain_video = VideoPlan {
				encoder: video.encoder.without_grain(),
				..video.clone()
			};
			let plain_target = video_quality::target(cx.job.speed, false, &cx.job.overrides);
			let (start, bytes) = match searched {
				Searched::Found(found) => (found.crf, Some(found.sample_bytes)),
				Searched::Unreachable { .. } => (quality.start(), None),
			};
			let plain = {
				let scorer = self.scorer(cx, workspace, &plain_video);
				Box::pin(video_quality::search_below(
					&scorer,
					samples,
					start,
					quality.aim(plain_target, inputs.declared_s),
					quality.slope,
					bytes,
					progress(1),
				))
				.await?
			};

			if video_quality::grain_choice(searched, plain) == GrainChoice::Plain {
				video.warnings.push(video_quality::dropped_grain_text(
					level,
					searched,
					plain,
					quality.target,
				));
				video.encoder = plain_video.encoder;
				quality.target = plain_target;
				quality.without_grain = true;
				searched = plain;
			}
		}

		let found = match searched {
			Searched::Found(found) => {
				quality.predicted = Some(found.score);
				found
			}
			Searched::Unreachable { crf, score } if quality.target.met(score) => {
				quality.predicted = Some(score);
				video.encoder = video.encoder.with_crf(crf);
				return Ok(Tuning::Encode { verify: true });
			}
			Searched::Unreachable { crf, score } if cx.job.options.forced_video(mergeable(inputs.info)) => {
				video.encoder = video.encoder.with_crf(crf);
				video.warnings.push(format!(
					"{}; encoded at CRF {crf} without a full check because it was requested",
					with_cap_note(
						&video_quality::short_text(crf, score, quality.target),
						video.max_bitrate_kbps
					)
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
		Ok(Self::predicted_size(cx, inputs, samples, found.sample_bytes))
	}

	/// Keeps the original when the found CRF's samples predict an output that isn't small enough.
	fn predicted_size(cx: &Context<'_>, inputs: &VideoInputs<'_>, samples: &[Sample], sample_bytes: u64) -> Tuning {
		if !cx.job.options.forced_video(mergeable(inputs.info))
			&& let Some(duration_s) = inputs
				.declared_s
				.filter(|duration| *duration >= video_encode::MIN_PREDICTED_DURATION_S)
		{
			let other = video_encode::other_bitrate(inputs.info, inputs.streams);
			let predicted = video_encode::predicted_bytes(sample_bytes, samples, duration_s, other);
			if let Some(reason) = video_encode::predicted_keep(inputs.compared_bytes, predicted, cx.job.min_savings) {
				return Tuning::Keep(reason);
			}
		}
		Tuning::Encode { verify: true }
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
		let mut first_offset = None;

		loop {
			(cx.on_event)(JobEvent::Stage(Stage::Verify));
			let score = {
				let scorer = self.scorer(cx, workspace, video);
				let on_fraction = |fraction| (cx.on_event)(JobEvent::Progress(fraction));
				Box::pin(scorer.verify(&temps.video.path, &windows, temps.encoded.frames, on_fraction)).await?
			};

			if attempt == 1 {
				first_offset = quality.predicted.map(|predicted| Offset {
					mean: score.mean - predicted.mean,
					p5: score.p5 - predicted.p5,
				});
			}

			let crf = video.encoder.crf();
			if quality.target.met(score) {
				return Ok(Checked::Passed(Observation {
					setting: search::implied_crf(crf, score, quality.target, quality.slope, bounds),
					slope: Some(quality.slope),
					bpp: output_bpp(&temps.video.path, video, temps.encoded.frames),
					offset: first_offset,
					without_grain: quality.without_grain,
				}));
			}

			let lower = if attempt <= MAX_RETRIES && video.max_bitrate_kbps.is_none() {
				search::retry_crf(crf, score, quality.target, quality.slope, bounds)
			} else {
				None
			};
			let Some(lower) = lower else {
				if cx.job.options.forced_video(mergeable(inputs.info)) {
					temps.warnings.push(short_note(
						&video_quality::short_text(crf, score, quality.target),
						video.max_bitrate_kbps,
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
		let fps = info
			.streams
			.iter()
			.find(|stream| stream.is_real_video())
			.and_then(Stream::fps);
		let warning = video_encode::check_video_temp(video, encoded, declared_s, start_s, fps)?;
		Ok((Temp { path, info }, warning))
	}

	async fn sample_gate(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		inputs: &VideoInputs<'_>,
		video: &VideoPlan,
	) -> Result<Option<KeepReason>, JobError> {
		let samples = video_encode::samples(inputs.declared_s);
		let Some(duration_s) = inputs.declared_s.filter(|_| !samples.is_empty()) else {
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

		let other_bits_per_s = video_encode::other_bitrate(inputs.info, inputs.streams);
		let predicted = video_encode::predicted_bytes(sample_bytes, &samples, duration_s, other_bits_per_s);
		Ok(video_encode::predicted_keep(
			inputs.compared_bytes,
			predicted,
			cx.job.min_savings,
		))
	}

	async fn cut_cues(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		job: &VideoJob<'_>,
	) -> Result<Vec<CueFile>, JobError> {
		let Some(window) = job.video.window else {
			return Ok(Vec::new());
		};

		let tracks: Vec<(u32, CueFormat)> = job
			.streams
			.subtitles
			.iter()
			.filter_map(|track| Some((track.stream_index, track.clipped?)))
			.collect();
		let paths = cues::cut(
			&self.tools.ffmpeg,
			&cx.job.source,
			workspace,
			&tracks,
			(window, job.info.format.start_time),
			cx.cancel,
		);

		Ok(tracks
			.iter()
			.zip(Box::pin(paths).await?)
			.map(|(&(origin, format), path)| CueFile { origin, path, format })
			.collect())
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
		let (chapter_source, chapter_count) = video_chapters(&chapter_file, job.info, strip, job.video.window)?;
		let cues = Box::pin(self.cut_cues(cx, workspace, job)).await?;

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
				cues: &cues,
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
		let declared_s = job.main.declared_duration().or(job.info.format.duration);
		let share = source_share(cx.source_bytes, job.video.window, declared_s);
		if !saves_enough(share.unwrap_or(cx.source_bytes), bytes, cx.job.min_savings) {
			if !cx.job.options.forced_video(mergeable(job.info)) {
				return Ok(Produced::Kept(KeepReason::NotSmaller {
					src_bytes: share.unwrap_or(cx.source_bytes),
					out_bytes: bytes,
				}));
			}

			warnings.push(not_smaller_warning(cx.source_bytes, share, bytes));
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
		let file_start_s = job.info.format.start_time.unwrap_or(0.0);
		let window_start_s = job.video.window.map(|window| window.start_s);
		check_sync(
			&self.tools,
			(source, &output),
			&synced_streams(&outputs, job.info, &job.streams.merge_from),
			(file_start_s + window_start_s.unwrap_or(0.0), window_start_s.is_some()),
			sync_tolerance_s(job.main.fps()),
			cx.cancel,
		)
		.await?;
		check_frame_times(
			&self.tools,
			(source, &output),
			job.main.index,
			FrameTiming {
				retiming: Retiming::of(job.scan),
				window: job.video.window.map(|window| TrimWindow {
					start_s: file_start_s + window.start_s,
					..window
				}),
				discontinuous: discontinuous(job.info),
			},
			cx.cancel,
		)
		.await?;
		if job.video.alpha {
			check_alpha(alpha_ssim(&self.tools, &output, source, job.video, cx.cancel).await?)?;
		}

		let warnings = fold_repeats(warnings);
		for warning in &warnings {
			(cx.on_event)(JobEvent::Warning(warning.clone()));
		}
		Ok(Produced::Output(Staged {
			path: output,
			bytes,
			source_share: share,
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
		inputs: &VideoInputs<'_>,
	) -> Result<EncodedTemps, JobError> {
		let (streams, declared_s) = (inputs.streams, inputs.declared_s);
		let video_path = workspace.file("video.a1.mkv");
		let audio_temp = workspace.file("audio.mka");
		let audio_plan = video_encode::audio_plan(inputs.info, streams, video.window);
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

/// `--resize`, or the size the picture is shown at when its pixels aren't square: players show an animated AVIF's
/// pixels square.
fn animation_size(resize: Option<Resize>, main: &Stream) -> Result<Option<(u32, u32)>, String> {
	let shown = video_plan::shown_size(main);
	let square = main
		.sample_aspect_ratio
		.is_none_or(|ratio| (ratio - 1.0).abs() < SQUARE_PIXEL_SLACK);

	match resize {
		Some(resize) => resize.size(shown, false).map(Some),
		None if square => Ok(None),
		None => Resize {
			width: None,
			height: None,
		}
		.size(shown, false)
		.map(Some),
	}
}

/// The frames a clip gives from its length and rate, after deinterlacing to field rate or removing a telecine.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn estimated_frames(span: Span, fps: Option<f64>, scan_type: ScanType) -> Option<u64> {
	let per_source_frame = match scan_type {
		ScanType::Progressive => 1.0,
		ScanType::Interlaced { .. } => 2.0,
		ScanType::Telecined => 0.8,
	};
	Some((span.length_s? * fps? * per_source_frame).round().max(0.0) as u64)
}

fn check_frame_count(frames: Option<u64>) -> Result<(), JobError> {
	match frames.and_then(animated::too_many_frames) {
		Some(reason) => Err(ErrorKind::EncodeFailed(reason).into()),
		None => Ok(()),
	}
}

/// The prepared frames counted uncompressed: FFV1 saves little on noisy frames.
fn frames_bytes(frames: u64, video: &VideoPlan, format: FrameFormat) -> u64 {
	frames
		.saturating_mul(u64::from(video.width) * u64::from(video.height))
		.saturating_mul(format.bytes_per_pixel)
}

fn frames_space_error(need: u64, free: u64, mut log_tail: Vec<String>) -> JobError {
	log_tail.push(FRAMES_SPACE_HINT.to_owned());
	JobError::with_log(ErrorKind::InsufficientSpace { need, free }, log_tail)
}

fn ignored_flags(job: &Job) -> Option<String> {
	let flags: Vec<&str> = [
		("--crf", job.options.advanced.crf.is_some()),
		("--video-encoder", job.options.advanced.video_encoder.is_some()),
		("--video-vmaf", job.overrides.video.is_some()),
		("--merge-audio", job.options.merge_audio),
		("--strip", job.options.strip),
	]
	.into_iter()
	.filter_map(|(flag, given)| given.then_some(flag))
	.collect();
	if flags.is_empty() {
		return None;
	}

	let verb = if flags.len() == 1 { "doesn't" } else { "don't" };
	Some(format!(
		"{} {verb} apply to a video written as an animated AVIF",
		flags.join(", ")
	))
}

fn dropped_streams(info: &MediaInfo, burned: Option<Burn>) -> Vec<String> {
	info.streams
		.iter()
		.filter_map(|stream| match stream.codec_type.as_str() {
			"audio" => Some(AUDIO_DROPPED),
			"subtitle" if burned.is_none_or(|burn| burn.stream != stream.index) => Some(SUBTITLES_DROPPED),
			_ => None,
		})
		.map(str::to_owned)
		.collect()
}

fn video_span(job: &Job, info: &MediaInfo, main: &Stream) -> Result<Span, JobError> {
	let declared_s = main.declared_duration().or(info.format.duration);
	let window = job
		.options
		.trim
		.map(|trim| trim.video_window(declared_s, main.fps()))
		.transpose()
		.map_err(ErrorKind::EncodeFailed)?;

	Ok(Span {
		declared_s,
		window,
		length_s: window.map_or(declared_s, |window| Some(window.length_s)),
	})
}

fn burn_pick(job: &Job, info: &MediaInfo) -> Result<Option<Burn>, JobError> {
	job.options
		.burn_subs
		.as_ref()
		.map(|burn_subs| burn::pick(info, &burn_subs.pick))
		.transpose()
		.map_err(|reason| ErrorKind::EncodeFailed(reason).into())
}

fn stream_rules(job: &Job, info: &MediaInfo) -> Result<StreamRules, JobError> {
	let burn = burn_pick(job, info)?;

	Ok(StreamRules {
		container: video_container(job),
		overrides: job.overrides,
		strip: job.options.strip,
		trim: job.options.trim.is_some(),
		cut_start: job.options.trim.is_some_and(|trim| trim.start_s > 0.0),
		cut_end: job.options.trim.is_some_and(|trim| trim.end_s.is_some()),
		merge: job.options.merge_audio,
		burn,
		..StreamRules::new(job.speed, job.keep_metadata)
	})
}

/// Only Matroska names its tracks in their `title`: elsewhere a track's `title` is the song's.
fn encoded_titles(info: &MediaInfo, tracks: &[TrackJob]) -> Box<[(u32, String)]> {
	if !info.format.format_name.contains("matroska") {
		return Box::default();
	}

	tracks
		.iter()
		.filter_map(|track| {
			let TrackPlan::Encode(encoded) = &track.plan else {
				return None;
			};
			let stream = info.streams.iter().find(|stream| stream.index == track.stream_index)?;
			Some((track.stream_index, titles::encoded_title(stream, encoded)?))
		})
		.collect()
}

fn with_clipped_chapters(plan: &FilePlan, info: &MediaInfo, workspace: &Workspace) -> Result<FilePlan, JobError> {
	let mut plan = plan.clone();
	if let Some(window) = plan.options.window.filter(|_| !plan.options.strip) {
		let path = workspace.file(CHAPTER_FILE);
		let clipped = chapters::clip(&info.chapters, window.on_source(info.format.start_time));
		write_chapter_file(&path, &clipped)?;
		plan.options.chapters = Some(path);
	}
	Ok(plan)
}

fn video_chapters(
	path: &Path,
	info: &MediaInfo,
	strip: bool,
	window: Option<TrimWindow>,
) -> Result<(ChapterSource, usize), JobError> {
	let window = match (strip, window) {
		(true, _) => return Ok((ChapterSource::Dropped, 0)),
		(false, None) => return Ok((ChapterSource::Source, info.chapters.len())),
		(false, Some(window)) => window,
	};

	let clipped = chapters::clip(&info.chapters, window.on_source(info.format.start_time));
	if clipped.is_empty() {
		return Ok((ChapterSource::Dropped, 0));
	}

	write_chapter_file(path, &clipped)?;
	Ok((ChapterSource::File, clipped.len()))
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

/// A merged track is only as long as its first source (`amix duration=first`), so its shortfall is checked
/// against that source's own declared duration, clipped to the trim window, not the container or the longest track.
fn merge_shortfall(info: &MediaInfo, merge_from: &[u32], window: Option<TrimWindow>, decoded_s: f64) -> Option<String> {
	let declared = merge_from
		.first()
		.and_then(|&index| info.streams.iter().find(|stream| stream.index == index))
		.and_then(Stream::declared_duration);

	match (declared, window) {
		(Some(declared), Some(window)) => {
			let length_s = (declared - window.start_s).max(0.0).min(window.length_s);
			TrimWindow { length_s, ..window }.shortfall(decoded_s)
		}
		(Some(declared), None) => (declared - decoded_s > (declared * 0.02).max(1.0))
			.then(|| format!("decodes to {decoded_s:.1} s but declares {declared:.1} s")),
		(None, Some(window)) => window.shortfall(decoded_s),
		(None, None) => None,
	}
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
	if !err.is_disk_full() || matches!(err.kind, ErrorKind::InsufficientSpace { .. }) {
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
			src_bytes: staged.source_share.unwrap_or(meta.len()),
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

fn short_note(reason: &str, cap_kbps: Option<u32>) -> String {
	format!(
		"{}; it was written because it was requested",
		with_cap_note(reason, cap_kbps)
	)
}

fn with_cap_note(reason: &str, cap_kbps: Option<u32>) -> String {
	match cap_kbps {
		Some(kbps) => format!("{reason} under the {} Mbit/s cap", f64::from(kbps) / 1000.0),
		None => reason.to_owned(),
	}
}

#[allow(clippy::cast_precision_loss)]
fn output_bpp(path: &Path, video: &VideoPlan, frames: u64) -> Option<f64> {
	let bytes = fs::metadata(path).ok()?.len();
	let pixels = f64::from(video.width) * f64::from(video.height) * frames as f64;
	(pixels > 0.0).then(|| bytes as f64 * 8.0 / pixels)
}

/// A trimmed output is weighed against the window's share of the source, not the whole file.
#[allow(
	clippy::cast_precision_loss,
	clippy::cast_possible_truncation,
	clippy::cast_sign_loss
)]
fn source_share(source_bytes: u64, window: Option<TrimWindow>, duration_s: Option<f64>) -> Option<u64> {
	let (window, duration_s) = (window?, duration_s.filter(|duration| *duration > 0.0)?);
	Some((source_bytes as f64 * (window.length_s / duration_s).min(1.0)).round() as u64)
}

fn not_smaller_warning(source_bytes: u64, share: Option<u64>, out_bytes: u64) -> String {
	match share {
		Some(part_bytes) => kept_as_requested_part(part_bytes, out_bytes),
		None => kept_as_requested(source_bytes, out_bytes),
	}
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

fn thread_budget(engine: Option<usize>, job_cap: Option<usize>, fallback: usize) -> usize {
	let engine = engine.unwrap_or(fallback);
	job_cap.map_or(engine, |cap| cap.min(engine)).max(1)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_engine_budget_replaces_the_fallback_and_a_job_cap_only_lowers_it() {
		assert_eq!(thread_budget(None, None, 16), 16);
		assert_eq!(thread_budget(Some(4), None, 16), 4);
		assert_eq!(thread_budget(Some(4), Some(2), 16), 2);
		assert_eq!(thread_budget(Some(4), Some(8), 16), 4);
		assert_eq!(thread_budget(None, Some(8), 16), 8);
		assert_eq!(thread_budget(None, Some(0), 16), 1);
	}

	#[test]
	fn capped_notes_name_the_cap_in_megabits() {
		assert_eq!(with_cap_note("missed", None), "missed");
		assert_eq!(
			with_cap_note("missed", Some(12_500)),
			"missed under the 12.5 Mbit/s cap"
		);
		assert_eq!(with_cap_note("missed", Some(60)), "missed under the 0.06 Mbit/s cap");
		assert_eq!(
			short_note("missed", None),
			"missed; it was written because it was requested"
		);
		assert_eq!(
			short_note("missed", Some(12_500)),
			"missed under the 12.5 Mbit/s cap; it was written because it was requested"
		);
	}

	#[test]
	fn a_clip_counts_its_frames_after_deinterlacing_or_removing_a_telecine() {
		let ten_seconds = Span {
			declared_s: Some(60.0),
			window: None,
			length_s: Some(10.0),
		};
		let unknown = Span {
			length_s: None,
			..ten_seconds
		};

		assert_eq!(
			estimated_frames(ten_seconds, Some(25.0), ScanType::Progressive),
			Some(250)
		);
		assert_eq!(
			estimated_frames(ten_seconds, Some(25.0), ScanType::Interlaced { top_first: true }),
			Some(500)
		);
		assert_eq!(
			estimated_frames(ten_seconds, Some(29.97), ScanType::Telecined),
			Some(240)
		);
		assert_eq!(estimated_frames(unknown, Some(25.0), ScanType::Progressive), None);
		assert_eq!(estimated_frames(ten_seconds, None, ScanType::Progressive), None);
		assert!(check_frame_count(Some(100_000)).is_ok());
		assert!(check_frame_count(None).is_ok());
		assert!(check_frame_count(Some(100_001)).is_err());
	}

	#[test]
	fn the_frames_are_counted_uncompressed_and_a_full_disk_names_that_need() {
		let video = VideoPlan {
			width: 1920,
			height: 1080,
			..crate::video::plan::test_plan()
		};
		let eight_bit = FrameFormat {
			pix_fmt: "bgr0",
			bytes_per_pixel: 3,
		};
		let full = frames_space_error(
			7_000_000,
			2_000,
			vec!["av_interleaved_write_frame(): No space left on device".into()],
		);

		assert_eq!(frames_bytes(100, &video, eight_bit), 622_080_000);
		assert_eq!(frames_bytes(u64::MAX, &video, eight_bit), u64::MAX);
		assert_eq!(
			full.kind,
			ErrorKind::InsufficientSpace {
				need: 7_000_000,
				free: 2_000
			}
		);
		assert!(full.log_tail.last().unwrap().contains("--ss/--to"));
		let kept = disk_full(full, Path::new("."), 10);
		assert_eq!(
			kept.kind,
			ErrorKind::InsufficientSpace {
				need: 7_000_000,
				free: 2_000
			}
		);
	}

	#[test]
	fn video_only_flags_are_named_once() {
		let job = |options: JobOptions, overrides: Overrides| Job {
			source: PathBuf::from("clip.mp4"),
			speed: Speed::Moderate,
			keep_metadata: false,
			drop_aux: false,
			min_savings: 0.1,
			overrides,
			options,
		};
		let flagged = JobOptions {
			strip: true,
			advanced: crate::options::Advanced {
				crf: Some(30.0),
				..crate::options::Advanced::default()
			},
			..JobOptions::default()
		};
		let merged = JobOptions {
			merge_audio: true,
			..JobOptions::default()
		};

		assert_eq!(ignored_flags(&job(JobOptions::default(), Overrides::default())), None);
		assert_eq!(
			ignored_flags(&job(flagged, Overrides::default())).as_deref(),
			Some("--crf, --strip don't apply to a video written as an animated AVIF")
		);
		assert_eq!(
			ignored_flags(&job(merged, Overrides::default())).as_deref(),
			Some("--merge-audio doesn't apply to a video written as an animated AVIF")
		);
	}
}
