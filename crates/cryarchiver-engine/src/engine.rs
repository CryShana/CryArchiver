use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::audio::plan::{AudioPlan, FilePlan, plan_file};
use crate::audio::{cover, encode, verify};
use crate::commit::{Artifact, CommitError, OutputSink, SourceTimes, check_space, free_space};
use crate::container::Cover;
use crate::detect::{self, Detection, MediaClass};
use crate::images::job::{self as image_job, ImageCaps, ImageContext, ImageInput, ImageOutcome};
use crate::integrity;
use crate::metadata::MetadataMode;
use crate::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason};
use crate::preset::Speed;
use crate::probe::{self, Frame, MediaInfo, ProbeError, Stream};
use crate::quality::keys::{CacheKey, Group, VideoGroup};
use crate::quality::memory::{GroupStats, Observation, QualityMemory, SharedMemory};
use crate::quality::search::{self, DEFAULT_SLOPE, MAX_PROBES, Target};
use crate::text::display_path;
use crate::toolchain::{MediaKind, Toolchain};
use crate::video::analyze::Analysis;
use crate::video::encode::{EncodedVideo, Sample};
use crate::video::mux::{self, Attachment, MuxInputs, MuxPlan};
use crate::video::plan::{self as video_plan, Encoder, VideoCaps, VideoPlan, plan_video, precheck};
use crate::video::quality::{self as video_quality, MAX_RETRIES, Scorer, Searched, Strategy};
use crate::video::streams::{Extracted, StreamPlan, plan_streams};
use crate::video::verify::{
	Expected, Timelines, alpha_ssim, check_alpha, earliest_video_start, probe_temp, start_offsets, verify_output,
};
use crate::video::{analyze, encode as video_encode};
use crate::workspace::Workspace;

const SOURCE_METADATA: &str = "source-metadata.json";
const MEMORY_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub struct Job {
	pub source: PathBuf,
	pub speed: Speed,
	pub keep_metadata: bool,
	pub drop_aux: bool,
	pub min_savings: f64,
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
}

enum Produced {
	Output(Staged),
	Kept(KeepReason),
}

enum Planned {
	Audio { info: Box<MediaInfo>, plan: FilePlan },
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
		let animated = matches!(class, MediaClass::AnimatedImage(_));
		let planned = match class {
			MediaClass::Audio(info) => {
				self.require(MediaKind::Audio)?;
				on_event(JobEvent::Stage(Stage::Plan));
				match plan_file(&info, job.speed) {
					AudioPlan::Encode(plan) => Planned::Audio {
						info: Box::new(info),
						plan,
					},
					AudioPlan::Keep(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
				}
			}
			MediaClass::Video(info) => {
				self.require(MediaKind::Video)?;
				let damage = integrity::find_damage(&job.source)
					.map_err(|err| JobError::io(IoSide::Source(&job.source), &err))?;
				if let Some(reason) = damage {
					return Err(ErrorKind::SourceDamaged(reason).into());
				}

				on_event(JobEvent::Stage(Stage::Plan));
				let planned =
					precheck(&info, &self.video_caps()).and_then(|_| plan_streams(&info, job.speed, job.keep_metadata));
				if let Err(reason) = planned {
					return Ok(JobOutcome::KeptOriginal { reason });
				}
				Planned::Video
			}
			MediaClass::StillImage(info) | MediaClass::AnimatedImage(info) => {
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
		let workspace = open_workspace(&work_root, &job.source, meta.len())?;

		let cx = Context {
			job,
			source_bytes: meta.len(),
			on_event,
			cancel,
		};
		let produced = match &planned {
			Planned::Audio { info, plan } => self.produce_audio(&cx, info, plan, &workspace).await,
			Planned::Video => Box::pin(self.produce_video(&cx, &workspace)).await,
			Planned::Image(input) => Box::pin(self.produce_image(&cx, input, &workspace)).await,
		};
		let staged = match produced.map_err(|err| disk_full(err, &work_root, cx.source_bytes))? {
			Produced::Output(staged) => staged,
			Produced::Kept(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
		};

		on_event(JobEvent::Stage(Stage::Commit));
		finish(job, sink, &work_root, &meta, before, staged)
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
			min_savings: cx.job.min_savings,
			mode: metadata_mode(cx.job),
			threads: self.image_threads,
			caps: self.image_caps(),
			on_event: cx.on_event,
			cancel: cx.cancel,
		};
		Ok(match image_job::produce(&image, input, workspace).await? {
			ImageOutcome::Output { path, bytes, warnings } => Produced::Output(Staged { path, bytes, warnings }),
			ImageOutcome::Kept(reason) => Produced::Kept(reason),
		})
	}

	fn video_caps(&self) -> VideoCaps {
		VideoCaps {
			vp9_alpha: self.tools.vp9_alpha,
			libaom: self.tools.has_libaom(),
		}
	}

	async fn produce_audio(
		&self,
		cx: &Context<'_>,
		info: &MediaInfo,
		plan: &FilePlan,
		workspace: &Workspace,
	) -> Result<Produced, JobError> {
		let output = workspace.file(&format!("out.{}", plan.container.extension));

		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let cmd = encode::command(&self.tools.ffmpeg, &cx.job.source, &output, plan);
		let on_fraction = |fraction| (cx.on_event)(JobEvent::Progress(fraction));
		let source_levels = encode::run(&cmd, plan, info.format.duration, on_fraction, cx.cancel).await?;

		let decoded_s = source_levels.iter().map(|levels| levels.duration_s).fold(0.0, f64::max);
		if let Some(reason) = info.duration_shortfall(decoded_s) {
			return Err(ErrorKind::SourceDamaged(reason).into());
		}

		(cx.on_event)(JobEvent::Stage(Stage::Metadata));
		let mut warnings = Vec::new();
		if plan.container.cover == Cover::VorbisPicture
			&& let Some(warning) = copy_cover(&cx.job.source, &output, info)
		{
			(cx.on_event)(JobEvent::Warning(warning.clone()));
			warnings.push(warning);
		}

		let bytes = fs::metadata(&output)
			.map_err(|err| JobError::io(IoSide::Destination(&output), &err))?
			.len();
		if !saves_enough(cx.source_bytes, bytes, cx.job.min_savings) {
			return Ok(Produced::Kept(KeepReason::NotSmaller {
				src_bytes: cx.source_bytes,
				out_bytes: bytes,
			}));
		}

		(cx.on_event)(JobEvent::Stage(Stage::Verify));
		let output_levels = verify::measure_output(&self.tools, &output, plan, cx.cancel).await?;
		verify::check_tracks(plan, &source_levels, &output_levels)
			.map_err(|reason| ErrorKind::EncodeFailed(format!("verification failed: {reason}")))?;

		Ok(Produced::Output(Staged {
			path: output,
			bytes,
			warnings,
		}))
	}

	async fn produce_video(&self, cx: &Context<'_>, workspace: &Workspace) -> Result<Produced, JobError> {
		let source = cx.job.source.as_path();

		(cx.on_event)(JobEvent::Stage(Stage::Probe));
		let (info, probe_json) = probe::probe_video(&self.tools, source, cx.source_bytes, cx.cancel)
			.await
			.map_err(video_probe_error)?;
		let planned = precheck(&info, &self.video_caps())
			.and_then(|main| plan_streams(&info, cx.job.speed, cx.job.keep_metadata).map(|streams| (main, streams)));
		let (main, streams) = match planned {
			Ok(planned) => planned,
			Err(reason) => return Ok(Produced::Kept(reason)),
		};
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
		let declared_s = main.declared_duration().or(info.format.duration);
		let segments = video_quality::analysis_segments(cx.job.speed);
		let analysis = Box::pin(analyze::analyze(
			&self.tools,
			source,
			main,
			declared_s,
			segments,
			cx.cancel,
		))
		.await?;
		let mut video = plan_video(&info, main, &frames, &analysis, cx.job.speed);
		let mut quality = self.video_quality(cx.job.speed, main, &analysis, &video);
		if cx.job.speed != Speed::Fast && !self.tools.has_vmaf() {
			video
				.warnings
				.push("libvmaf is not available: the video quality was not measured".to_owned());
		}

		let learned_bpp = quality.stats.and_then(|stats| stats.bpp);
		if let Some(reason) = learned_bpp.and_then(|bpp| video_plan::efficient_at(&info, main, bpp)) {
			return Ok(Produced::Kept(KeepReason::AlreadyEfficient(reason)));
		}

		let inputs = VideoInputs {
			info: &info,
			streams: &streams,
			declared_s,
		};
		let verify =
			match Box::pin(self.choose_crf(cx, workspace, &inputs, &analysis, &mut video, &mut quality)).await? {
				Tuning::Keep(reason) => return Ok(Produced::Kept(reason)),
				Tuning::Encode { verify } => verify,
			};

		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let mut temps = Box::pin(self.encode_stage(cx, workspace, source, &video, &streams, declared_s)).await?;
		if verify
			&& let Some(reason) =
				Box::pin(self.check_quality(cx, workspace, &inputs, &mut video, &mut temps, &quality)).await?
		{
			return Ok(Produced::Kept(reason));
		}

		let job = VideoJob {
			info: &info,
			probe_json: &probe_json,
			first_frame: frames.first(),
			main,
			video: &video,
			streams: &streams,
		};
		Box::pin(self.mux_and_verify(cx, workspace, &job, temps)).await
	}

	fn video_quality(&self, speed: Speed, main: &Stream, analysis: &Analysis, video: &VideoPlan) -> VideoQuality {
		let target = video_quality::target(speed, video.encoder.film_grain());
		let key = CacheKey {
			group: Group::Video(VideoGroup::of(main, analysis, video)),
			encoder: self.encoder_id(&video.encoder),
			preset: video.encoder.preset(),
			target: target.mean,
			floor: Some(target.floor),
		};
		let stats = self.quality.lookup(&key);

		VideoQuality {
			strategy: video_quality::strategy(speed, stats.as_ref(), video.encoder.crf(), self.tools.has_vmaf()),
			slope: stats.and_then(|stats| stats.slope).unwrap_or(DEFAULT_SLOPE),
			key,
			stats,
			target,
		}
	}

	fn encoder_id(&self, encoder: &Encoder) -> String {
		match encoder {
			Encoder::SvtAv1 { .. } => self
				.tools
				.svt_av1
				.map_or_else(|| "svt-av1".to_owned(), |version| format!("svt-av1-{version}")),
			Encoder::Aom { .. } => "libaom-av1".to_owned(),
			Encoder::Vp9Alpha { .. } => "libvpx-vp9".to_owned(),
		}
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
		let samples = video_quality::search_samples(analysis, inputs.declared_s, cx.job.speed);
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
		let gate = Box::pin(self.sample_gate(cx, workspace, inputs.info, video, inputs.streams, inputs.declared_s));
		Ok(match gate.await? {
			Some(reason) => Tuning::Keep(reason),
			None => Tuning::Encode { verify },
		})
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

		if let Some(duration_s) = inputs
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
	) -> Result<Option<KeepReason>, JobError> {
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
				let bpp = output_bpp(&temps.video.path, video, temps.encoded.frames);
				let observation = Observation {
					setting: crf,
					slope: Some(quality.slope),
					bpp,
				};
				self.quality.record(&quality.key, observation);
				return Ok(None);
			}

			let lower = if attempt <= MAX_RETRIES {
				search::retry_crf(crf, score, quality.target, quality.slope, bounds)
			} else {
				None
			};
			let Some(lower) = lower else {
				return Ok(Some(video_quality::unreachable_reason(crf, score, quality.target)));
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
		let metadata = cx.job.keep_metadata.then_some((job.probe_json, job.first_frame));
		let attachments = collect_attachments(workspace, &job.streams.extracted, metadata, &mut warnings)?;

		let output = workspace.file("mux.mkv");
		let cmd = mux::command(
			&self.tools.ffmpeg,
			&MuxInputs {
				video: &temps.video.path,
				audio: temps.audio.as_ref().map(|temp| temp.path.as_path()),
				source,
				source_bytes: cx.source_bytes,
				attachments: &attachments,
			},
			&MuxPlan {
				info: job.info,
				video: job.video,
				streams: job.streams,
				keep_metadata: cx.job.keep_metadata,
			},
			&output,
		);
		mux::mux(&cmd, cx.cancel).await?;

		let bytes = fs::metadata(&output)
			.map_err(|err| JobError::io(IoSide::Destination(&output), &err))?
			.len();
		if !saves_enough(cx.source_bytes, bytes, cx.job.min_savings) {
			return Ok(Produced::Kept(KeepReason::NotSmaller {
				src_bytes: cx.source_bytes,
				out_bytes: bytes,
			}));
		}

		(cx.on_event)(JobEvent::Stage(Stage::Verify));
		let timelines = Timelines {
			source: job.info,
			video_temp: &temps.video.info,
			audio_temp: temps.audio.as_ref().map(|temp| &temp.info),
		};
		let expected = Expected {
			video: job.video,
			streams: job.streams,
			source_chapters: job.info.chapters.len(),
			attached_files: attachments.len(),
			start_offsets: start_offsets(&mux::output_streams(job.video, job.streams), &timelines),
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
		let audio_plan = video_encode::audio_plan(streams);
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

fn open_workspace(work_root: &Path, source: &Path, source_bytes: u64) -> Result<Workspace, JobError> {
	check_space(work_root, source_bytes)?;
	Workspace::create(work_root, source).map_err(|err| JobError::io(IoSide::Destination(work_root), &err))
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
