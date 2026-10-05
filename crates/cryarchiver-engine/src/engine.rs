use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;

use crate::audio::plan::{
	AudioPlan, AudioRules, FilePlan, StreamTags, TrackJob, TrackPlan, merge_delays, merge_file, mergeable,
	mixed_languages_warning, plan_file_with, retarget, unusable,
};
use crate::audio::{cover, encode, tags, verify};
use crate::budget::{Hold, Lanes, ThreadBudget, Width, cores};
use crate::calibration::tables;
use crate::commit::{Artifact, CommitError, OutputSink, SourceTimes, check_space, free_space};
use crate::container::{Container, Cover, MATROSKA_VIDEO, MP4_VIDEO};
use crate::decision::{
	AudioDecision, AudioTrack, CrfChoice, Decision, Measured, Recorder, Reencode, Scan, SkippedRetry, Trim, Vmaf,
	encoder_text,
};
use crate::detect::{self, Detection, MediaClass};
use crate::format::{OutputFormat, check_bitrate_cap, check_kind};
use crate::images::animated::{self, Animation};
use crate::images::job::{self as image_job, ImageCaps, ImageContext, ImageInput, ImageOutcome};
use crate::images::plan::ImageRoute;
use crate::integrity;
use crate::metadata::MetadataMode;
use crate::options::{JobOptions, Resize, TrimWindow, WrittenBy, kept_as_requested, kept_as_requested_part, upscaled};
use crate::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason, Refusal};
use crate::preset::{Overrides, Speed};
use crate::probe::{self, Frame, MediaInfo, ProbeError, Stream};
use crate::quality::keys::{CacheKey, Group, VideoGroup};
use crate::quality::memory::{GroupStats, Observation, Offset, QualityMemory, SharedMemory};
use crate::quality::search::{self, Bounds, MAX_PROBES, Probe, Target};
use crate::quality::vmaf::Score;
use crate::text::{display_path, fold_repeats};
use crate::toolchain::{LIBAOM, LIBVPX_VP9, MediaKind, SVT_AV1, Toolchain};
use crate::video::analyze::{Analysis, ScanType};
use crate::video::burn::{self, Burn};
use crate::video::colour::FrameFormat;
use crate::video::encode::{EncodedVideo, Sample};
use crate::video::mux::{self, Attachment, CLEAN_STREAM_TAGS, ChapterSource, CueFile, MuxInputs, MuxPlan};
use crate::video::plan::{
	self as video_plan, Encoder, VideoCaps, VideoPlan, plan_video, precheck, threads_for, usable_main,
};
use crate::video::quality::{
	self as video_quality, Aim, GrainChoice, MAX_RETRIES, Scorer, Search, SearchStep, Searched, Strategy, with_cap_note,
};
use crate::video::streams::{CueFormat, Extracted, StreamPlan, StreamRules, plan_streams};
use crate::video::verify::{
	Expected, FrameTiming, Retiming, Timelines, alpha_ssim, check_alpha, check_frame_times, check_sync,
	earliest_video_start, probe_temp, start_offsets, sync_tolerance_s, synced_streams, undecoded_lead, verify_output,
};
use crate::video::{
	InputStart, VideoStarts, analyze, chapters, colour, cues, discontinuous, encode as video_encode, titles,
};
use crate::workspace::Workspace;

const SOURCE_METADATA: &str = "source-metadata.json";
const CHAPTER_FILE: &str = "chapters.txt";
const MEMORY_POLL: Duration = Duration::from_millis(250);
const ANIMATION_FRAMES: &str = "frames.mkv";
const FRAMES_SAMPLE: &str = "frames-sample.mkv";
/// The prepared frames' size is measured on this many frames at the middle of each of this many equal parts of the
/// span. The busiest sample is given this much room for busier scenes, and the estimate never drops below this share
/// of the frames' uncompressed size.
const FRAMES_PER_SAMPLE: u64 = 2;
const FRAME_SAMPLES: u32 = 8;
const FRAMES_SPACE_FACTOR: u64 = 2;
const MIN_FRAMES_SHARE: u64 = 8;
const SQUARE_PIXEL_SLACK: f64 = 1e-3;
const AUDIO_DROPPED: &str = "an audio track was dropped: animated AVIF has no sound";
const SUBTITLES_DROPPED: &str =
	"a subtitle track was dropped: animated AVIF can't hold subtitles, but --burn-subs draws one into the picture";
const FRAMES_SPACE_HINT: &str = "an animated AVIF is made from lossless frames stored in the work folder: use \
                                 --ss/--to for a shorter clip or --resize for smaller frames";

#[derive(Debug, Clone, Default)]
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
	/// `done` of the current step's `total` parts are finished: the analysis's segments, a probe's samples, or the
	/// predicted-size gate's samples.
	Parts {
		done: usize,
		total: usize,
	},
	/// The quality search starts probe `number` of at most `most`, at `crf`.
	Probe {
		number: usize,
		most: usize,
		crf: f64,
	},
	/// The probe at `crf` scored `score` over its samples.
	Probed {
		crf: f64,
		score: Score,
	},
	EncodeAgain(EncodeAgain),
	/// What the engine decided for the file, once its output is ready to commit or it keeps the original.
	Decision(Box<Decision>),
}

/// The full encode is repeated because its quality check missed the target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeAgain {
	pub attempt: u32,
	pub attempts: u32,
	pub measured: Score,
	pub target: Target,
	pub from_crf: f64,
	pub crf: f64,
}

impl EncodeAgain {
	/// The warning filed with the outcome for this repeat.
	#[must_use]
	pub fn warning(&self) -> String {
		format!(
			"the quality check measured VMAF {:.1} / {:.1} at CRF {}; the video was encoded again at CRF {}",
			self.measured.mean, self.measured.p5, self.from_crf, self.crf
		)
	}
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

/// How many videos encode at once, across every engine given the same slots.
#[derive(Debug)]
pub struct VideoSlots {
	semaphore: Semaphore,
	jobs: usize,
}

impl VideoSlots {
	pub fn new(jobs: usize) -> Self {
		Self {
			semaphore: Semaphore::new(jobs.max(1)),
			jobs: jobs.max(1),
		}
	}

	pub fn jobs(&self) -> usize {
		self.jobs
	}
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
	video_slots: Arc<VideoSlots>,
	budget: Arc<ThreadBudget>,
	image_threads: usize,
	metric_threads: usize,
	video_threads: Option<usize>,
	memory_gate: Option<Arc<dyn MemoryGate>>,
	quality: Arc<dyn QualityMemory>,
	/// The expected full encode time from which a learned CRF is searched again.
	long_encode_s: f64,
}

struct Context<'a> {
	job: &'a Job,
	source_bytes: u64,
	on_event: &'a (dyn Fn(JobEvent) + Sync),
	cancel: &'a CancellationToken,
	record: &'a Recorder,
}

struct Staged {
	path: PathBuf,
	bytes: u64,
	/// Under a trim, the window's share of the source: the size the output is weighed and reported against.
	source_share: Option<u64>,
	warnings: Vec<String>,
	/// Recorded only once the output is committed.
	learned: Vec<(CacheKey, Lesson)>,
}

enum Produced {
	Output(Staged),
	Kept(KeepReason),
}

enum Planned {
	Audio {
		info: Box<MediaInfo>,
		plan: Box<FilePlan>,
		from_video: bool,
		/// Warnings for the audio tracks of a video that can't be extracted and were left out.
		skipped: Vec<String>,
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

struct VideoResources<'a> {
	_slot: SemaphorePermit<'a>,
	_memory: Option<MemoryReservation<'a>>,
	hold: Hold<'a>,
	/// The memory the job reserved, which each tool run beside its own needs too.
	lane_bytes: u64,
}

struct VideoInputs<'a> {
	lanes: Lanes<'a>,
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
	/// How far the group's full checks land from its samples; it outlives a forgotten setting.
	offset: Option<Offset>,
	/// What the search samples scored at the CRF the video is encoded at.
	predicted: Option<Score>,
	/// The probes of the search pass the video is encoded from: a missed check's retry aims on them.
	curve: Vec<Probe>,
	/// The bytes of each of `curve`'s probes: a retry's size is predicted from them.
	curve_bytes: Vec<u64>,
}

impl VideoQuality {
	/// Takes `search`'s probes as the curve the video is encoded from; returns its result.
	fn follow(&mut self, search: Search) -> Searched {
		self.curve = search.probes;
		self.curve_bytes = search.sample_bytes;
		search.searched
	}

	fn start(&self) -> f64 {
		match self.strategy {
			Strategy::Search { start } => start,
			Strategy::Fixed { crf, .. } => crf,
		}
	}

	/// A search toward `nominal` for a full encode of `length_s`, moved by what the group learned about its samples,
	/// at an encoding rate measured earlier, if any.
	fn aim(&self, nominal: Target, length_s: Option<f64>, rate: Option<f64>) -> Aim {
		Aim {
			nominal,
			search: video_quality::search_target(nominal, self.offset),
			length_s,
			rate,
		}
	}
}

/// What the size gate's samples showed.
struct Gate {
	keep: Option<KeepReason>,
	/// The full encode is expected to take `Engine::long_encode_s` or more at the samples' rate.
	long: bool,
}

enum Tuning {
	Keep(KeepReason),
	Encode { verify: bool },
}

enum Checked {
	Passed(Observation),
	/// The original is kept: the encode missed its check and can't be retried, or its retry is already too large.
	Missed(KeepReason),
	/// Missed by at most half a point and written: its offset is learned, its setting forgotten.
	NearMiss(Observation),
	/// Written below the target because it was requested; learns nothing.
	Short,
}

/// What a committed output teaches its group.
enum Lesson {
	Setting(Observation),
	/// Only how far the full check landed from the samples.
	Offset(Observation),
}

impl Engine {
	pub fn new(tools: Toolchain) -> Self {
		let cores = cores();
		Self {
			tools,
			video_slots: Arc::new(VideoSlots::new(1)),
			budget: Arc::new(ThreadBudget::new(cores)),
			image_threads: cores,
			metric_threads: cores,
			video_threads: None,
			memory_gate: None,
			quality: Arc::new(SharedMemory::default()),
			long_encode_s: video_quality::long_encode_s(),
		}
	}

	#[must_use]
	pub fn with_quality_memory(self, quality: Arc<dyn QualityMemory>) -> Self {
		Self { quality, ..self }
	}

	/// Moves the expected full encode time from which a learned CRF is searched again (the tables' `margin-from-s` by
	/// default).
	#[must_use]
	pub fn with_long_encode_s(self, long_encode_s: f64) -> Self {
		Self { long_encode_s, ..self }
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
		self.with_video_slots(Arc::new(VideoSlots::new(jobs)))
	}

	#[must_use]
	pub fn with_video_slots(self, video_slots: Arc<VideoSlots>) -> Self {
		Self { video_slots, ..self }
	}

	/// Shares `budget` with the other engines given it; `with_thread_budget` then resizes it for all of them.
	#[must_use]
	pub fn with_shared_thread_budget(self, budget: Arc<ThreadBudget>) -> Self {
		Self { budget, ..self }
	}

	/// The threads all of the engine's jobs share (the whole machine by default). A video job's measuring widens
	/// only into the part of it that the other jobs leave free.
	#[must_use]
	pub fn with_thread_budget(self, threads: usize) -> Self {
		self.budget.resize(threads);
		self
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

	/// The threads the engine's jobs share, and what they hold of them.
	pub fn thread_budget(&self) -> &ThreadBudget {
		&self.budget
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

		let recorder = Recorder::default();
		let recording = |event: JobEvent| {
			recorder.observe(&event);
			on_event(event);
		};
		let on_event: &(dyn Fn(JobEvent) + Sync) = &recording;

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
							merge_file(*plan, &info, job.speed, &job.overrides)
						} else {
							*plan
						};
						let mut plan = retarget(plan, job.options.format, &info).map_err(ErrorKind::EncodeFailed)?;
						plan.options.strip = job.options.strip;
						plan.options.window = trim_window(job, info.format.duration)?;
						align_merge(&mut plan, &info);

						Planned::Audio {
							plan: Box::new(plan),
							info: Box::new(info),
							from_video: false,
							skipped: Vec::new(),
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
				let (info, skipped) = extractable(info);
				match plan_file_with(&info, job.speed, &job.overrides, rules) {
					AudioPlan::Encode(plan) => {
						let plan = if job.options.merge_audio {
							merge_file(*plan, &info, job.speed, &job.overrides)
						} else {
							*plan
						};
						let mut plan =
							extracted(retarget(plan, job.options.format, &info).map_err(ErrorKind::EncodeFailed)?);
						plan.options.strip = job.options.strip;
						plan.options.window = trim_window(job, info.format.duration)?;
						plan.options.clean_tags = (!job.keep_metadata).then(|| clean_tags(&info, &plan.tracks));
						align_merge(&mut plan, &info);

						Planned::Audio {
							plan: Box::new(plan),
							info: Box::new(info),
							from_video: true,
							skipped,
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
					.and_then(|main| plan_streams(&info, &rules).map(|_| main));
				match planned {
					Ok(main) => {
						self.check_libaom(job, main)?;
						Planned::Video(rules)
					}
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
			record: &recorder,
		};
		let produced = match &planned {
			Planned::Audio {
				info,
				plan,
				from_video,
				skipped,
			} => {
				self.produce_audio(&cx, info, plan, (*from_video, skipped), &workspace)
					.await
			}
			Planned::Video(rules) => Box::pin(self.produce_video(&cx, &workspace, *rules)).await,
			Planned::Animation(burn) => Box::pin(self.produce_video_animation(&cx, &workspace, *burn)).await,
			Planned::Image(input) => Box::pin(self.produce_image(&cx, input, &workspace)).await,
		};
		let image = matches!(planned, Planned::Image(_) | Planned::Animation(_));
		let ran_on = attempt_threads(image, self.video_threads, self.image_threads, job.options.threads);
		let produced = produced.map_err(|err| disk_full(err, &work_root, cx.source_bytes).ran_on(ran_on));
		let mut staged = match produced? {
			Produced::Output(staged) => staged,
			Produced::Kept(reason) => {
				recorder.update(|decision| {
					decision.source_bytes.get_or_insert(cx.source_bytes);
				});
				on_event(JobEvent::Decision(Box::new(recorder.snapshot())));
				return Ok(JobOutcome::KeptOriginal { reason });
			}
		};
		let learned = std::mem::take(&mut staged.learned);

		recorder.update(|decision| {
			decision.warnings.clone_from(&staged.warnings);
			decision.source_bytes = Some(staged.source_share.unwrap_or(cx.source_bytes));
			decision.output_bytes = Some(staged.bytes);
			decision.attempts = decision.attempts.max(1);
		});
		let decision = Box::new(recorder.snapshot());
		on_event(JobEvent::Decision(decision.clone()));

		on_event(JobEvent::Stage(Stage::Commit));
		let outcome = finish(job, sink, &work_root, &meta, before, staged, decision)?;

		if matches!(outcome, JobOutcome::Encoded { .. }) {
			for (key, lesson) in &learned {
				match lesson {
					Lesson::Setting(observation) => self.quality.record(key, *observation),
					Lesson::Offset(observation) => self.quality.record_offset(key, *observation),
				}
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
		let threads = self.job_threads(cx.job, self.image_threads);
		let _hold = self.budget.hold(threads.min(self.budget.total()));

		let image = ImageContext {
			tools: &self.tools,
			source: &cx.job.source,
			source_bytes: cx.source_bytes,
			source_is_part: false,
			speed: cx.job.speed,
			overrides: cx.job.overrides,
			min_savings: cx.job.min_savings,
			mode: metadata_mode(cx.job),
			threads,
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
				chosen,
			} => {
				cx.record.update(|decision| decision.image = Some(chosen));
				Produced::Output(Staged {
					path,
					bytes,
					source_share: None,
					warnings,
					learned: learned
						.into_iter()
						.map(|(key, observation)| (key, Lesson::Setting(observation)))
						.collect(),
				})
			}
			ImageOutcome::Kept(reason) => Produced::Kept(reason),
		})
	}

	fn video_caps(&self) -> VideoCaps {
		VideoCaps {
			vp9_alpha: self.tools.vp9_alpha,
		}
	}

	fn video_main<'a>(&self, info: &'a MediaInfo, job: &Job) -> Result<&'a Stream, KeepReason> {
		if job.options.forced_video(mergeable(info)) {
			usable_main(info, &self.video_caps())
		} else {
			precheck(info, &self.video_caps())
		}
	}

	/// Refuses a video that needs libaom-av1 when this ffmpeg lacks it, before anything is decoded.
	fn check_libaom(&self, job: &Job, main: &Stream) -> Result<(), JobError> {
		let choice = job.options.advanced.video_encoder;
		if let Some(problem) = self.tools.video_encoder_problem(choice) {
			return Err(ErrorKind::Toolchain(problem.into()).into());
		}

		match video_plan::beyond_svt(main, job.options.resize) {
			Some((width, height)) if choice.is_none() && !self.tools.has_libaom() => Err(ErrorKind::Toolchain(
				format!("{width}×{height} is larger than SVT-AV1 supports and needs ffmpeg with libaom-av1"),
			)
			.into()),
			_ => Ok(()),
		}
	}

	/// A video slot, then the memory for decoding at the source's size and at the output's, and the job's share of the
	/// thread budget.
	async fn video_resources(
		&self,
		cx: &Context<'_>,
		main: &Stream,
		size: Option<(u32, u32)>,
	) -> Result<VideoResources<'_>, JobError> {
		(cx.on_event)(JobEvent::Stage(Stage::Queued));
		let slot = tokio::select! {
			slot = self.video_slots.semaphore.acquire() => slot.expect("the semaphore is never closed"),
			() = cx.cancel.cancelled() => return Err(ErrorKind::Cancelled.into()),
		};

		let decoded = video_plan::memory_estimate(main.width.unwrap_or(0), main.height.unwrap_or(0));
		let estimate = size.map_or(decoded, |(width, height)| {
			decoded.max(video_plan::memory_estimate(width, height))
		});
		let memory = self.reserve_memory(estimate, cx.on_event, cx.cancel).await?;
		let share = self
			.tool_threads(cx.job)
			.unwrap_or_else(|| self.budget.total() / self.video_slots.jobs);
		Ok(VideoResources {
			_slot: slot,
			_memory: memory,
			hold: self.budget.hold(share.max(1)),
			lane_bytes: estimate,
		})
	}

	/// Measuring within the job's own thread budget when it has one, and otherwise within its share of the engine's,
	/// widened into what the other jobs leave free.
	fn lanes<'a>(&'a self, job: &Job, resources: &'a VideoResources<'a>) -> Lanes<'a> {
		Lanes {
			width: self
				.tool_threads(job)
				.map_or(Width::Shared(&resources.hold), Width::Fixed),
			memory: self.memory_gate.as_deref(),
			lane_bytes: resources.lane_bytes,
		}
	}

	async fn analyze_main(
		&self,
		cx: &Context<'_>,
		main: &Stream,
		span: Span,
		lanes: Lanes<'_>,
	) -> Result<Analysis, JobError> {
		(cx.on_event)(JobEvent::Stage(Stage::Analyze));
		let segments = video_quality::analysis_segments(cx.job.speed, span.length_s);
		let on_segments = |done, total| (cx.on_event)(JobEvent::Parts { done, total });
		Box::pin(analyze::analyze_reporting(
			&self.tools,
			&cx.job.source,
			main,
			(span.declared_s, span.window),
			(segments, lanes),
			&on_segments,
			cx.cancel,
		))
		.await
	}

	/// The thread budget of the tools a video job starts, when the engine or the job sets one.
	fn tool_threads(&self, job: &Job) -> Option<usize> {
		(self.video_threads.is_some() || job.options.threads.is_some()).then(|| self.job_threads(job, cores()))
	}

	fn budget_threads(&self, job: &Job, video: &mut VideoPlan) {
		video.threads = self.tool_threads(job).map(threads_for);
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
		(from_video, skipped): (bool, &[String]),
		workspace: &Workspace,
	) -> Result<Produced, JobError> {
		// An audio encode keeps about one core busy.
		let _hold = self.budget.hold(1);

		let output = workspace.file(&format!("out.{}", plan.container.extension));
		let mut plan = with_clipped_chapters(plan, info, workspace)?;
		plan.options.titles = encoded_titles(info, &plan.tracks);
		let plan = &plan;
		cx.record.update(|decision| decision.audio = Some(audio_decision(plan)));

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
				Some(window) if from_video => within_declared(window, longest_audio_s(info)).shortfall(decoded_s),
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
		let mut warnings = skipped.to_vec();
		for warning in skipped {
			(cx.on_event)(JobEvent::Warning(warning.clone()));
		}

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

			let by = cx.job.options.written_by(mergeable(info));
			let warning = not_smaller_warning(cx.source_bytes, share, bytes, by);
			(cx.on_event)(JobEvent::Warning(warning.clone()));
			warnings.push(warning);
		}

		(cx.on_event)(JobEvent::Stage(Stage::Verify));
		let output_levels = verify::measure_output(&self.tools, &output, plan, cx.cancel).await?;
		verify::check_tracks(plan, &source_levels, &output_levels).map_err(JobError::verification)?;

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
		let (info, probe_json) =
			probe::probe_video(&self.tools, source, cx.source_bytes, counts_packets(cx.job), cx.cancel)
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
		let compared_bytes = source_share(cx.source_bytes, window, declared_s).unwrap_or(cx.source_bytes);
		cx.record
			.update(|decision| decision.source_bytes = Some(compared_bytes));

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

		let resources = self.video_resources(cx, main, size).await?;
		let lanes = self.lanes(cx.job, &resources);
		let analysis = Box::pin(self.analyze_main(cx, main, span, lanes)).await?;
		let mut video = plan_video(&info, main, &frames, &analysis, cx.job.speed, size);
		video.warnings.extend(size.and_then(|size| upscaled(shown, size)));
		self.budget_threads(cx.job, &mut video);

		if let Some(choice) = cx.job.options.advanced.video_encoder {
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

		cx.record.video(|record| record.grain = video.encoder.grain_level());
		record_picture(cx, analysis.scan, size, window, rules.burn.is_some());

		let mut quality = self.video_quality(cx.job, main, &analysis, &video, span_s);
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
			lanes,
			info: &info,
			streams: &streams,
			declared_s: span_s,
			span: window.or(declared_s.map(|length_s| TrimWindow { start_s: 0.0, length_s })),
			compared_bytes,
		};
		let tuning = Box::pin(self.choose_crf(cx, workspace, &inputs, &analysis, &mut video, &mut quality)).await?;
		record_choice(cx, &video, &quality);
		let verify = match tuning {
			Tuning::Keep(reason) => return Ok(self.kept_video(&quality, reason)),
			Tuning::Encode { verify } => verify,
		};

		let _uncapped = lanes.uncapped_stage();
		cx.record.update(|decision| decision.attempts = 1);
		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let mut temps = Box::pin(self.encode_stage(cx, workspace, source, &video, &inputs)).await?;

		if !cx.job.options.forced_video(mergeable(&info))
			&& let Some(reason) = temps_not_smaller(&temps, compared_bytes, cx.job.min_savings)
		{
			return Ok(self.kept_video(&quality, reason));
		}

		let teaches = window.is_none() && size.is_none();
		let learned = if verify {
			match Box::pin(self.check_quality(cx, workspace, &inputs, &mut video, &mut temps, &quality)).await? {
				Checked::Passed(observation) => teaches.then_some(Lesson::Setting(observation)),
				Checked::NearMiss(observation) => {
					self.forget_learned(&quality);
					teaches.then_some(Lesson::Offset(observation))
				}
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
				learned: learned.map(|lesson| (quality.key, lesson)).into_iter().collect(),
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
			return Err(ErrorKind::Toolchain(animated::NEEDS_LIBAOM.into()).into());
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
		let (info, _) = probe::probe_video(&self.tools, source, cx.source_bytes, counts_packets(cx.job), cx.cancel)
			.await
			.map_err(video_probe_error)?;
		let main = match self.video_main(&info, cx.job) {
			Ok(main) => main,
			Err(reason) => return Ok(Produced::Kept(reason)),
		};

		let span = video_span(cx.job, &info, main)?;
		check_fewest_frames(span, main.fps())?;

		let shown = video_plan::shown_size(main);
		let size = animation_size(cx.job.options.resize, main).map_err(ErrorKind::EncodeFailed)?;
		let frames = probe::first_frames(&self.tools, source, main.index, 5, cx.cancel)
			.await
			.map_err(video_probe_error)?;

		let resources = self.video_resources(cx, main, size).await?;
		let lanes = self.lanes(cx.job, &resources);
		let analysis = Box::pin(self.analyze_main(cx, main, span, lanes)).await?;
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
		if let Some(overlay) = &mut video.overlay {
			overlay.in_frame_format = true;
		}
		record_picture(cx, analysis.scan, size, span.window, burn.is_some());

		let mut warnings = dropped_streams(&info, burn);
		warnings.extend(ignored_flags(cx.job));
		warnings.extend(video.warnings.iter().cloned());
		warnings.extend(size.and_then(|size| upscaled(shown, size)));
		if cx.job.keep_metadata {
			warnings.push("metadata is not carried into animated AVIF yet".to_owned());
		}

		let share = source_share(cx.source_bytes, span.window, span.declared_s);
		let compared = share.unwrap_or(cx.source_bytes);
		cx.record.update(|decision| decision.source_bytes = Some(compared));

		let _uncapped = lanes.uncapped_stage();
		let frames_need = match frame_count {
			Some(frames) => {
				let raw = frames_bytes(frames, &video, format);
				Some(Box::pin(self.frames_need(cx, workspace, &video, (span, frames), raw, compared)).await?)
			}
			None => None,
		};

		let job = AnimationJob {
			video: &video,
			span_s: span.length_s,
			share,
			need: frames_need.map(|need| need.saturating_add(compared)),
		};
		Box::pin(self.animate(cx, workspace, &job, warnings)).await
	}

	/// What the `frames` prepared from `span` will take: `raw` (their uncompressed size) when the work disk holds that
	/// beside the `output` bytes anyway or when they can't be sampled, and otherwise a measure of FFV1 samples of them.
	async fn frames_need(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		video: &VideoPlan,
		(span, frames): (Span, u64),
		raw: u64,
		output: u64,
	) -> Result<u64, JobError> {
		let path = workspace.file(FRAMES_SAMPLE);
		if free_space(&path).is_ok_and(|free| free >= raw.saturating_add(output)) {
			return Ok(raw);
		}

		let measured = Box::pin(self.sample_frames(cx, &path, video, frame_samples(span, frames))).await;
		let _ = fs::remove_file(&path);
		Ok(measured?.map_or(raw, |measured| measured_frames_bytes(&measured, frames, raw)))
	}

	/// Each of `windows` prepared into `path` like the frames: its bytes and frames, or `None` when one can't be.
	async fn sample_frames(
		&self,
		cx: &Context<'_>,
		path: &Path,
		video: &VideoPlan,
		windows: Vec<TrimWindow>,
	) -> Result<Option<Vec<(u64, u64)>>, JobError> {
		let mut measured = Vec::new();

		for sample in windows {
			let plan = VideoPlan {
				window: Some(sample),
				expected_frames: None,
				..video.clone()
			};
			let cmd = video_encode::frames_command(&self.tools.ffmpeg, &cx.job.source, path, &plan);

			let encoded = Box::pin(video_encode::encode_video(
				&cmd,
				&plan,
				Some(sample.length_s),
				|_| {},
				cx.cancel,
			));
			let sampled = match encoded.await {
				Ok(encoded) => encoded.frames,
				Err(err) if err.kind == ErrorKind::Cancelled => return Err(err),
				Err(_) => return Ok(None),
			};

			let Ok(meta) = fs::metadata(path) else {
				return Ok(None);
			};
			measured.push((meta.len(), sampled));
		}
		Ok(Some(measured))
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
			alpha: video.alpha
				&& animated::has_alpha(
					&self.tools,
					&frames.path,
					self.job_threads(cx.job, self.image_threads),
					cx.cancel,
				)
				.await?,
			timeline,
			size: None,
			matrix: video.colour.matrix.clone(),
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
			source_is_part: job.share.is_some(),
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
				path,
				bytes,
				warnings,
				chosen,
				..
			} => {
				cx.record.update(|decision| decision.image = Some(chosen));
				Produced::Output(Staged {
					path,
					bytes,
					source_share: job.share,
					warnings,
					learned: Vec::new(),
				})
			}
			ImageOutcome::Kept(reason) => Produced::Kept(reason),
		})
	}

	fn kept_video(&self, quality: &VideoQuality, reason: KeepReason) -> Produced {
		if matches!(reason, KeepReason::NotSmaller { .. }) {
			self.forget_learned(quality);
		}
		Produced::Kept(reason)
	}

	/// A group whose learned setting led to an output that wasn't smaller, or that missed its check, even narrowly,
	/// forgets it, so that its next file searches afresh instead of repeating the same encodes.
	fn forget_learned(&self, quality: &VideoQuality) {
		if quality.stats.is_some() {
			self.quality.forget(&quality.key);
		}
	}

	fn video_quality(
		&self,
		job: &Job,
		main: &Stream,
		analysis: &Analysis,
		video: &VideoPlan,
		length_s: Option<f64>,
	) -> VideoQuality {
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
			let strategy = video_quality::strategy(
				job.speed,
				stats.as_ref(),
				video.encoder.crf(),
				self.tools.has_vmaf(),
				length_s,
			);
			(stats, strategy)
		};

		VideoQuality {
			strategy,
			slope: stats
				.and_then(|stats| stats.slope)
				.unwrap_or(tables().video.default_slope),
			offset: self.quality.offset(&key),
			key,
			stats,
			target,
			without_grain: false,
			predicted: None,
			curve: Vec::new(),
			curve_bytes: Vec::new(),
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

	fn scorer<'a>(
		&'a self,
		cx: &'a Context<'_>,
		workspace: &'a Workspace,
		video: &'a VideoPlan,
		lanes: Lanes<'a>,
	) -> Scorer<'a> {
		Scorer {
			tools: &self.tools,
			source: &cx.job.source,
			workspace,
			plan: video,
			threads: self.job_threads(cx.job, self.metric_threads),
			lanes,
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

		let gate = Box::pin(self.sample_gate(cx, workspace, inputs, video)).await?;
		let searchable = learned && !samples.is_empty();
		match gate.keep {
			None if searchable && gate.long => {
				quality.strategy = Strategy::Search { start: quality.start() };
				Box::pin(self.search_crf(cx, workspace, inputs, &samples, video, quality)).await
			}
			None => Ok(Tuning::Encode { verify }),
			Some(_) if searchable => {
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
		let probes_run = &AtomicUsize::new(0);
		let progress = |pass: usize| search_events(cx.on_event, (pass, passes), probes_run);
		let first = {
			let scorer = self.scorer(cx, workspace, video, inputs.lanes);
			let aim = quality.aim(quality.target, inputs.declared_s, None);
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
		let rate = first.rate;
		let mut searched = quality.follow(first);

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
			let grain_probes = cx.record.probe_count();

			let plain = {
				let scorer = self.scorer(cx, workspace, &plain_video, inputs.lanes);
				Box::pin(video_quality::search_below(
					&scorer,
					samples,
					start,
					quality.aim(plain_target, inputs.declared_s, Some(rate)),
					quality.slope,
					bytes,
					progress(1),
				))
				.await?
			};
			cx.record.tag_without_grain(grain_probes);

			if video_quality::grain_choice(searched, plain.searched) == GrainChoice::Plain {
				video.warnings.push(video_quality::dropped_grain_text(
					level,
					searched,
					plain.searched,
					quality.target,
					video.max_bitrate_kbps,
				));
				video.encoder = plain_video.encoder;
				quality.target = plain_target;
				quality.without_grain = true;
				searched = quality.follow(plain);
			}
		}

		let found = match searched {
			Searched::Found(found) => {
				quality.predicted = Some(found.score);
				found
			}
			Searched::Unreachable {
				crf,
				score,
				sample_bytes,
			} if quality.target.met(score) => {
				quality.predicted = Some(score);
				video.encoder = video.encoder.with_crf(crf);
				return Ok(Self::predicted_size(cx, inputs, samples, sample_bytes));
			}
			Searched::Unreachable { crf, score, .. } if cx.job.options.forced_video(mergeable(inputs.info)) => {
				video.encoder = video.encoder.with_crf(crf);
				let by = cx.job.options.video_written_by(mergeable(inputs.info));
				let short = video_quality::short_text(crf, score, quality.target);
				video
					.warnings
					.push(unchecked_text(&short, crf, video.max_bitrate_kbps, by));
				return Ok(Tuning::Encode { verify: false });
			}
			Searched::Unreachable { crf, score, .. } => {
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
		let bounds = video_quality::bounds(&video.encoder);
		let mut attempt = 1;
		let mut first_offset = None;

		loop {
			(cx.on_event)(JobEvent::Stage(Stage::Verify));
			let score = {
				let scorer = self.scorer(cx, workspace, video, inputs.lanes);
				let on_fraction = |fraction| (cx.on_event)(JobEvent::Progress(fraction));
				Box::pin(scorer.verify(
					&temps.video.path,
					temps.encoded.frames,
					temps.encoded.decoded_s,
					on_fraction,
				))
				.await?
			};

			if attempt == 1 {
				first_offset = quality.predicted.map(|predicted| Offset {
					mean: score.mean - predicted.mean,
					p5: score.p5 - predicted.p5,
				});
			}

			let crf = video.encoder.crf();
			cx.record.video(|record| record.checks.push(Measured::new(crf, score)));
			let passed = quality.target.met(score);
			if passed || video_quality::near_miss(score, quality.target) {
				let observation = Observation {
					setting: search::implied_crf(crf, score, quality.target, quality.slope, bounds),
					slope: Some(quality.slope),
					bpp: output_bpp(&temps.video.path, video, temps.encoded.frames),
					offset: first_offset,
					without_grain: quality.without_grain,
				};
				if passed {
					return Ok(Checked::Passed(observation));
				}

				temps.warnings.push(video_quality::near_miss_text(
					crf,
					score,
					quality.target,
					video.max_bitrate_kbps,
				));
				cx.record.video(|record| record.near_miss = true);
				return Ok(Checked::NearMiss(observation));
			}

			let lower = if attempt <= MAX_RETRIES && video.max_bitrate_kbps.is_none() {
				retry_at(quality, crf, score, bounds)
			} else {
				None
			};
			let Some(lower) = lower else {
				if cx.job.options.forced_video(mergeable(inputs.info)) {
					temps.warnings.push(short_note(
						&video_quality::short_text(crf, score, quality.target),
						video.max_bitrate_kbps,
						cx.job.options.video_written_by(mergeable(inputs.info)),
					));
					return Ok(Checked::Short);
				}

				return Ok(Checked::Missed(video_quality::unreachable_reason(
					crf,
					score,
					quality.target,
				)));
			};
			if let Some(reason) = skipped_retry(cx, inputs, quality, temps, (crf, lower)) {
				return Ok(Checked::Missed(reason));
			}

			video.encoder = video.encoder.with_crf(lower);
			attempt += 1;
			let repeat = EncodeAgain {
				attempt,
				attempts: MAX_RETRIES + 1,
				measured: score,
				target: quality.target,
				from_crf: crf,
				crf: lower,
			};
			temps.warnings.push(repeat.warning());
			record_retry(cx, &repeat, video.encoder.crf());

			(cx.on_event)(JobEvent::EncodeAgain(repeat));
			(cx.on_event)(JobEvent::Stage(Stage::Encode));
			Box::pin(self.reencode_video(cx, workspace, inputs.declared_s, video, temps, attempt)).await?;

			if !cx.job.options.forced_video(mergeable(inputs.info))
				&& let Some(reason) = temps_not_smaller(temps, inputs.compared_bytes, cx.job.min_savings)
			{
				return Ok(Checked::Missed(reason));
			}
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
		let fps = info.real_video_streams().next().and_then(Stream::fps);
		let warning = video_encode::check_video_temp(video, encoded, declared_s, start_s, fps)?;
		Ok((Temp { path, info }, warning))
	}

	async fn sample_gate(
		&self,
		cx: &Context<'_>,
		workspace: &Workspace,
		inputs: &VideoInputs<'_>,
		video: &VideoPlan,
	) -> Result<Gate, JobError> {
		let samples = video_encode::samples(inputs.declared_s);
		let Some(duration_s) = inputs.declared_s.filter(|_| !samples.is_empty()) else {
			return Ok(Gate {
				keep: None,
				long: false,
			});
		};

		(cx.on_event)(JobEvent::Stage(Stage::Sample));
		let on_samples = |done| {
			(cx.on_event)(JobEvent::Parts {
				done,
				total: samples.len(),
			});
		};
		let uncapped = inputs.lanes.uncapped_stage();
		let encoded = Box::pin(video_encode::encode_samples(
			&self.tools.ffmpeg,
			&cx.job.source,
			workspace,
			video,
			&samples,
			on_samples,
			cx.cancel,
		))
		.await?;
		drop(uncapped);

		let other_bits_per_s = video_encode::other_bitrate(inputs.info, inputs.streams);
		let predicted = video_encode::predicted_bytes(encoded.bytes, &samples, duration_s, other_bits_per_s);
		Ok(Gate {
			keep: video_encode::predicted_keep(inputs.compared_bytes, predicted, cx.job.min_savings),
			long: video_quality::long_encode(encoded.rate, duration_s, self.long_encode_s),
		})
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

			let by = cx.job.options.video_written_by(mergeable(job.info));
			warnings.push(not_smaller_warning(cx.source_bytes, share, bytes, by));
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
		verify::check_tracks(plan, source_levels, &output_levels).map_err(JobError::verification)
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

/// A search pass's steps as job events. Probes are numbered across the passes, each counting on from those run
/// before it, and `Progress` spreads the passes' most probes over the stage.
fn search_events<'a>(
	on_event: &'a (dyn Fn(JobEvent) + Sync),
	(pass, passes): (usize, usize),
	probes_run: &'a AtomicUsize,
) -> impl FnMut(SearchStep) + 'a {
	move |step| match step {
		SearchStep::Probe { round, crf } => {
			#[allow(clippy::cast_precision_loss)]
			on_event(JobEvent::Progress(
				(pass * MAX_PROBES + round) as f64 / (passes * MAX_PROBES) as f64,
			));

			let number = probes_run.fetch_add(1, Ordering::SeqCst) + 1;
			let earlier_probes = number - 1 - round;
			on_event(JobEvent::Probe {
				number,
				most: earlier_probes + (passes - pass) * MAX_PROBES,
				crf,
			});
		}
		SearchStep::Samples { done, total } => on_event(JobEvent::Parts { done, total }),
		SearchStep::Probed { crf, score } => on_event(JobEvent::Probed { crf, score }),
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

/// Before the analysis the scan is unknown, so only a clip over the limit at the fewest frames any scan gives (an
/// inverse telecine's) is refused.
fn check_fewest_frames(span: Span, fps: Option<f64>) -> Result<(), JobError> {
	check_frame_count(estimated_frames(span, fps, ScanType::Telecined))
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

/// Short windows at the middle of each of `FRAME_SAMPLES` equal parts of the span, each about `FRAMES_PER_SAMPLE` of
/// its `frames` long.
#[allow(clippy::cast_precision_loss)]
fn frame_samples(span: Span, frames: u64) -> Vec<TrimWindow> {
	let Some(length_s) = span.length_s.filter(|length_s| *length_s > 0.0) else {
		return Vec::new();
	};

	if frames == 0 {
		return Vec::new();
	}

	let start_s = span.window.map_or(0.0, |window| window.start_s);
	let sample_s = (length_s * FRAMES_PER_SAMPLE as f64 / frames as f64).min(length_s);
	let part_s = length_s / f64::from(FRAME_SAMPLES);
	(0..FRAME_SAMPLES)
		.map(|part| TrimWindow {
			start_s: start_s + (part_s * (f64::from(part) + 0.5) - sample_s / 2.0).clamp(0.0, length_s - sample_s),
			length_s: sample_s,
		})
		.collect()
}

/// The prepared frames' size from `samples` of them (their bytes and frames): the busiest sample's size per frame for
/// every frame, with room to spare, but between a share of what the frames take uncompressed (`raw`) and all of it.
fn measured_frames_bytes(samples: &[(u64, u64)], frames: u64, raw: u64) -> u64 {
	samples
		.iter()
		.filter(|(_, count)| *count > 0)
		.map(|(bytes, count)| bytes.div_ceil(*count))
		.max()
		.map_or(raw, |per_frame| {
			per_frame
				.saturating_mul(frames)
				.saturating_mul(FRAMES_SPACE_FACTOR)
				.clamp(raw / MIN_FRAMES_SHARE, raw)
		})
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
		forced: job.options.forced_video(mergeable(info)),
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

/// The packet count only gives the frames an untrimmed encode must have (`plan_window`); a trim reads its window alone.
fn counts_packets(job: &Job) -> bool {
	job.options.trim.is_none()
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

/// The video without the audio tracks that can't be encoded, and a warning for each one left out. When no track is
/// usable, the video as it is, which the plan then refuses with the reason.
fn extractable(info: MediaInfo) -> (MediaInfo, Vec<String>) {
	let mut usable = info.clone();
	let mut warnings = Vec::new();
	let mut track = 0;
	usable.streams.retain(|stream| {
		if !stream.is_audio() {
			return true;
		}

		track += 1;
		let Some(why) = unusable(stream) else {
			return true;
		};
		warnings.push(format!(
			"audio track {track} ({}) was left out: {why}",
			stream.codec_name
		));
		false
	});

	if usable.audio_streams().next().is_none() {
		return (info, Vec::new());
	}
	(usable, warnings)
}

fn align_merge(plan: &mut FilePlan, info: &MediaInfo) {
	let seek_s = plan.options.window.map_or(0.0, |window| window.start_s);
	let zero_s = InputStart::of(info, &plan.options.merge_from).at(seek_s);

	plan.options.merge_delays_s = merge_delays(info, &plan.options.merge_from, zero_s);
}

fn clean_tags(info: &MediaInfo, tracks: &[TrackJob]) -> StreamTags {
	tracks
		.iter()
		.filter_map(|track| info.streams.iter().find(|stream| stream.index == track.stream_index))
		.flat_map(|stream| {
			CLEAN_STREAM_TAGS
				.iter()
				.filter_map(|key| Some((stream.index, *key, stream.tag(key)?.to_owned())))
		})
		.collect()
}

fn extraction_refusal(reason: &KeepReason) -> String {
	match reason {
		KeepReason::Unsupported(why) => format!("the audio can't be extracted: {why}"),
		_ => "the audio can't be extracted".to_owned(),
	}
}

fn longest_audio_s(info: &MediaInfo) -> Option<f64> {
	let declared = info
		.audio_streams()
		.filter_map(Stream::declared_duration)
		.fold(0.0, f64::max);

	(declared > 0.0).then_some(declared)
}

/// The part of `window` that audio declared to last `declared_s` can fill.
fn within_declared(window: TrimWindow, declared_s: Option<f64>) -> TrimWindow {
	declared_s.map_or(window, |declared_s| TrimWindow {
		length_s: (declared_s - window.start_s).max(0.0).min(window.length_s),
		..window
	})
}

fn extraction_shortfall(info: &MediaInfo, decoded_s: f64) -> Option<String> {
	let declared = longest_audio_s(info)?;

	(declared - decoded_s > (declared * 0.02).max(1.0))
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
		(Some(declared), Some(window)) => within_declared(window, Some(declared)).shortfall(decoded_s),
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
	decision: Box<Decision>,
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
			decision,
		}),
		Err(CommitError::Exists(path)) => Ok(skipped(&format!("output already exists: {}", display_path(&path)))),
		Err(CommitError::IsSource(path)) => Ok(skipped(&format!("{} is the source file", display_path(&path)))),
		Err(CommitError::Io(err)) => Err(JobError::io(
			IoSide::Destination(work_root.parent().unwrap_or(work_root)),
			&err,
		)),
	}
}

/// How the CRF the video is encoded at was chosen, and the target its check holds it to.
fn record_choice(cx: &Context<'_>, video: &VideoPlan, quality: &VideoQuality) {
	let learned = |crf: f64| {
		quality
			.stats
			.is_some_and(|stats| (stats.setting - crf).abs() < f64::EPSILON)
	};
	let chosen = match quality.strategy {
		_ if cx.job.options.advanced.crf.is_some() => CrfChoice::Fixed,
		Strategy::Search { .. } if quality.stats.is_some() => CrfChoice::LearnedSearchedAgain,
		Strategy::Search { .. } => CrfChoice::Searched,
		Strategy::Fixed { crf, verify: true } if learned(crf) => CrfChoice::Learned,
		Strategy::Fixed { crf, verify: false } if learned(crf) => CrfChoice::LearnedUnmeasured,
		Strategy::Fixed { verify: true, .. } => CrfChoice::Unsearched,
		Strategy::Fixed { verify: false, .. } => CrfChoice::Table,
	};
	let tables = &tables().video;

	cx.record.video(|record| {
		record.encoder = encoder_text(&video.encoder);
		record.crf = Some(video.encoder.crf());
		record.chosen = Some(chosen);
		record.target = Some(Vmaf::from(quality.target));
		record.grain_allowance = video.encoder.film_grain().then_some(Vmaf {
			mean: tables.grain_allowance_mean,
			p5: tables.grain_allowance_floor,
		});
		record.custom_target = cx.job.speed != Speed::Fast && cx.job.overrides.normalized().video.is_some();
		record.grain_dropped = quality.without_grain;
	});
}

/// What was done to the picture: its scan, a resize, a trim and burned subtitles.
fn record_picture(
	cx: &Context<'_>,
	scan: ScanType,
	resized: Option<(u32, u32)>,
	window: Option<TrimWindow>,
	burned: bool,
) {
	cx.record.video(|record| {
		record.scan = Some(Scan::from(scan));
		record.deinterlaced = scan != ScanType::Progressive;
		record.resized = resized;
		record.trim = window.map(Trim::from);
		record.burned_subtitles = burned;
	});
}

fn audio_decision(plan: &FilePlan) -> AudioDecision {
	let tracks = plan
		.tracks
		.iter()
		.map(|track| match &track.plan {
			TrackPlan::Encode(encode) => AudioTrack {
				codec: "opus".into(),
				bitrate_kbps: Some(encode.bitrate_kbps),
				channels: Some(encode.channels),
			},
			TrackPlan::Copy => AudioTrack {
				codec: "copy".into(),
				..AudioTrack::default()
			},
		})
		.collect();
	AudioDecision { tracks }
}

fn unsearched_warning(declared_s: Option<f64>, crf: f64) -> String {
	let why = if declared_s.is_some() {
		"no part of the video could be sampled"
	} else {
		"the video's duration is unknown"
	};
	format!("the CRF was not searched because {why}; the video was encoded at CRF {crf} and checked")
}

fn short_note(reason: &str, cap_kbps: Option<u32>, by: WrittenBy) -> String {
	format!("{}; {}", with_cap_note(reason, cap_kbps), by.note())
}

/// An unreachable search encoded at its closest `crf` without a full check, because the output was forced.
fn unchecked_text(short: &str, crf: f64, cap_kbps: Option<u32>, by: WrittenBy) -> String {
	format!(
		"{}; encoded at CRF {crf} without a full check {}",
		with_cap_note(short, cap_kbps),
		by.because()
	)
}

/// The record of a second encode at `again`, the CRF its encoder was given for `repeat`.
fn record_retry(cx: &Context<'_>, repeat: &EncodeAgain, again: f64) {
	let (score, target) = (repeat.measured, repeat.target);

	cx.record.video(|record| {
		record.reencode = Some(Reencode {
			reason: format!(
				"the full check measured VMAF {:.1} / {:.1}, below the target {} / {}",
				score.mean, score.p5, target.mean, target.floor
			),
			from_crf: repeat.from_crf,
			to_crf: again,
		});
		record.crf = Some(again);
	});
	cx.record.update(|decision| decision.attempts = repeat.attempt);
}

/// Why the original is kept without the retry from `crf` to `lower`, when the job isn't forced and the retry's size,
/// predicted from the search probes' bytes and the first encode's, would clearly fail the size rule. The record says
/// so. Without probes (a learned setting), nothing is predicted and the retry runs.
fn skipped_retry(
	cx: &Context<'_>,
	inputs: &VideoInputs<'_>,
	quality: &VideoQuality,
	temps: &EncodedTemps,
	(crf, lower): (f64, f64),
) -> Option<KeepReason> {
	if cx.job.options.forced_video(mergeable(inputs.info)) {
		return None;
	}

	let probe_sizes: Vec<(f64, u64)> = quality
		.curve
		.iter()
		.map(|probe| probe.crf)
		.zip(quality.curve_bytes.iter().copied())
		.collect();
	let first = fs::metadata(&temps.video.path).ok()?.len();
	let audio = temps
		.audio
		.as_ref()
		.and_then(|temp| fs::metadata(&temp.path).ok())
		.map_or(0, |meta| meta.len());
	let predicted = video_encode::retry_bytes(&probe_sizes, (crf, first), lower)?.saturating_add(audio);
	let reason = video_encode::retry_keep(inputs.compared_bytes, predicted, cx.job.min_savings)?;

	cx.record.video(|record| {
		record.skipped_retry = Some(SkippedRetry {
			from_crf: crf,
			to_crf: lower,
			predicted_bytes: predicted,
		});
	});
	Some(reason)
}

/// The CRF a missed check at `crf` is encoded again at: on the search's probe curve, or else on its slope.
fn retry_at(quality: &VideoQuality, crf: f64, score: Score, bounds: Bounds) -> Option<f64> {
	quality
		.predicted
		.and_then(|predicted| search::curve_retry_crf(crf, score, predicted, &quality.curve, quality.target, bounds))
		.or_else(|| search::retry_crf(crf, score, quality.target, quality.slope, bounds))
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

fn not_smaller_warning(source_bytes: u64, share: Option<u64>, out_bytes: u64, by: WrittenBy) -> String {
	match share {
		Some(part_bytes) => kept_as_requested_part(part_bytes, out_bytes, by),
		None => kept_as_requested(source_bytes, out_bytes, by),
	}
}

/// The encoded temps already save too little on `compared_bytes`: the mux only adds the copied streams to them, and
/// checking them, which can only lead to a lower CRF, only makes them larger.
fn temps_not_smaller(temps: &EncodedTemps, compared_bytes: u64, min_savings: f64) -> Option<KeepReason> {
	let bytes = std::iter::once(&temps.video)
		.chain(&temps.audio)
		.filter_map(|temp| fs::metadata(&temp.path).ok())
		.map(|meta| meta.len())
		.sum();

	(!saves_enough(compared_bytes, bytes, min_savings)).then_some(KeepReason::NotSmaller {
		src_bytes: compared_bytes,
		out_bytes: bytes,
	})
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

/// The thread budget an attempt ran on: an image's encoder threads, or else what the video tools got, the whole
/// machine without a budget.
fn attempt_threads(image: bool, engine: Option<usize>, image_threads: usize, job_cap: Option<usize>) -> usize {
	let fallback = if image { image_threads } else { cores() };
	thread_budget(engine, job_cap, fallback)
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
	fn an_attempt_ran_on_the_image_share_or_the_video_tools_budget() {
		let cores = cores();

		assert_eq!(attempt_threads(true, None, 4, None), 4);
		assert_eq!(attempt_threads(true, None, 4, Some(2)), 2);
		assert_eq!(attempt_threads(true, Some(3), 3, None), 3);
		assert_eq!(attempt_threads(false, None, 4, None), cores);
		assert_eq!(attempt_threads(false, Some(2), 4, None), 2);
		assert_eq!(attempt_threads(false, None, 4, Some(1)), 1);
	}

	const SVT: Bounds = Bounds {
		min: 10.0,
		max: 55.0,
		step: 0.25,
	};
	const TARGET: Target = Target {
		mean: 95.0,
		floor: 90.0,
	};

	fn score(mean: f64, p5: f64) -> Score {
		Score { mean, p5, frames: 300 }
	}

	fn probes(points: &[(f64, f64, f64)]) -> Vec<Probe> {
		points
			.iter()
			.map(|&(crf, mean, p5)| Probe {
				crf,
				score: score(mean, p5),
			})
			.collect()
	}

	fn quality(predicted: Option<Score>, curve: Vec<Probe>) -> VideoQuality {
		let group = VideoGroup {
			codec: crate::quality::keys::CodecFamily::H264,
			resolution: crate::video::plan::Resolution::Sd,
			fps: crate::quality::keys::FpsBucket::UpTo31,
			range: crate::quality::keys::DynamicRange::Sdr8,
			scan: ScanType::Progressive,
			grain: None,
			noisy: false,
			unmeasured: false,
			alpha: false,
		};

		VideoQuality {
			key: CacheKey {
				group: Group::Video(group),
				encoder: "svt-av1".to_owned(),
				preset: 5,
				target: TARGET.mean,
				floor: Some(TARGET.floor),
			},
			stats: None,
			target: TARGET,
			strategy: Strategy::Search { start: 30.0 },
			slope: 0.5,
			without_grain: false,
			offset: None,
			predicted,
			curve_bytes: vec![0; curve.len()],
			curve,
		}
	}

	#[test]
	fn a_retry_follows_the_probe_curve_when_there_is_one_and_the_slope_otherwise() {
		let curve = probes(&[(22.0, 97.0, 94.0), (26.0, 96.0, 92.5), (30.0, 95.0, 91.0)]);
		let (full, predicted) = (score(93.6, 89.6), score(95.0, 91.0));
		let on_slope = search::retry_crf(30.0, full, TARGET, 0.5, SVT);

		let curved = retry_at(&quality(Some(predicted), curve.clone()), 30.0, full, SVT);

		assert_eq!(
			curved,
			search::curve_retry_crf(30.0, full, predicted, &curve, TARGET, SVT)
		);
		assert_ne!(curved, on_slope, "{curved:?}");
		assert_eq!(
			retry_at(&quality(Some(predicted), Vec::new()), 30.0, full, SVT),
			on_slope
		);
		assert_eq!(retry_at(&quality(None, curve), 30.0, full, SVT), on_slope);
	}

	#[test]
	fn a_retry_follows_the_probes_of_the_pass_the_video_is_encoded_from() {
		let search = |points: &[(f64, f64, f64)]| Search {
			searched: Searched::Unreachable {
				crf: 30.0,
				score: score(95.0, 91.0),
				sample_bytes: 1,
			},
			rate: 1.0,
			probes: probes(points),
			sample_bytes: vec![1_000; points.len()],
		};
		let grain = search(&[(20.0, 99.0, 98.0), (30.0, 95.0, 91.0)]);
		let plain = search(&[(28.0, 96.6, 92.6), (30.0, 95.0, 91.0)]);
		let full = score(94.0, 90.0);
		let mut chosen = quality(Some(score(95.0, 91.0)), Vec::new());

		chosen.follow(grain);
		chosen.follow(plain.clone());

		assert_eq!(chosen.curve, plain.probes);
		assert_eq!(chosen.curve_bytes, plain.sample_bytes);
		assert_eq!(
			retry_at(&chosen, 30.0, full, SVT),
			search::curve_retry_crf(30.0, full, score(95.0, 91.0), &plain.probes, TARGET, SVT)
		);
	}

	#[test]
	fn a_retry_aims_on_the_worse_metric_at_each_probe() {
		let curve = probes(&[
			(24.0, 98.5, 92.5),
			(26.0, 97.5, 91.2),
			(28.0, 96.0, 91.0),
			(30.0, 95.0, 91.0),
		]);

		let retried = retry_at(&quality(Some(score(95.0, 91.0)), curve), 30.0, score(94.0, 90.0), SVT);

		assert_eq!(
			retried,
			Some(25.5),
			"the floor misses at 26 though the mean passes there; the mean alone would give 27.25"
		);
	}

	#[test]
	fn a_retry_takes_the_first_crossing_below_on_a_curve_that_dips() {
		let curve = probes(&[
			(24.0, 98.5, 99.0),
			(28.0, 96.3, 99.0),
			(30.0, 97.0, 99.0),
			(34.0, 94.5, 93.0),
		]);

		let retried = retry_at(&quality(Some(score(94.5, 93.0)), curve), 34.0, score(93.5, 92.0), SVT);

		assert_eq!(
			retried,
			Some(30.75),
			"between 34 and 30, not on the far side of the dip at 28"
		);
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
			short_note("missed", None, WrittenBy::Trim),
			"missed; it was written because it was trimmed"
		);
		assert_eq!(
			short_note("missed", Some(12_500), WrittenBy::Request),
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
	fn before_the_analysis_only_a_clip_too_long_even_after_removing_a_telecine_is_refused() {
		let span = |minutes: f64| Span {
			declared_s: Some(minutes * 60.0),
			window: None,
			length_s: Some(minutes * 60.0),
		};

		assert!(check_fewest_frames(span(58.0), Some(29.97)).is_ok());
		assert!(check_fewest_frames(span(70.0), Some(29.97)).is_err());
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
	fn the_frames_are_estimated_from_their_busiest_sample_with_room_to_spare_between_a_share_of_uncompressed_and_all() {
		let busiest = FRAMES_SPACE_FACTOR * 20_000 * 1_000;

		assert_eq!(
			measured_frames_bytes(&[(40_000, 4), (80_000, 4), (8_000, 4)], 1_000, 200_000_000),
			busiest
		);
		assert_eq!(
			measured_frames_bytes(&[(80_000, 4)], 1_000, 1_000_000_000),
			1_000_000_000 / MIN_FRAMES_SHARE
		);
		assert!(busiest < 1_000_000_000 / MIN_FRAMES_SHARE);
		assert_eq!(measured_frames_bytes(&[(40_000, 4)], 1_000, 1_000_000), 1_000_000);
		assert_eq!(measured_frames_bytes(&[(40_000, 0)], 1_000, 5_000_000), 5_000_000);
		assert_eq!(measured_frames_bytes(&[], 1_000, 5_000_000), 5_000_000);
	}

	#[test]
	fn the_frame_samples_are_spread_inside_the_trimmed_span() {
		let trimmed = Span {
			declared_s: Some(60.0),
			window: Some(TrimWindow {
				start_s: 10.0,
				length_s: 20.0,
			}),
			length_s: Some(20.0),
		};
		let close = |found: f64, wanted: f64| (found - wanted).abs() < 1e-9;

		let samples = frame_samples(trimmed, 500);

		let two_frames_s = 20.0 * 2.0 / 500.0;
		assert_eq!(samples.len(), 8, "{samples:?}");
		for (sample, centre) in samples
			.iter()
			.zip([11.25, 13.75, 16.25, 18.75, 21.25, 23.75, 26.25, 28.75])
		{
			assert!(
				close(sample.start_s, centre - two_frames_s / 2.0) && close(sample.length_s, two_frames_s),
				"{samples:?}"
			);
		}
		assert!(
			frame_samples(
				Span {
					length_s: None,
					..trimmed
				},
				500
			)
			.is_empty()
		);
		assert!(frame_samples(trimmed, 0).is_empty());
		assert!(
			frame_samples(trimmed, 2)
				.iter()
				.all(|sample| close(sample.start_s, 10.0) && close(sample.length_s, 20.0))
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
