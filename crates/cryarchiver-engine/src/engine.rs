use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::audio::plan::{AudioPlan, FilePlan, plan_file};
use crate::audio::{cover, encode, verify};
use crate::commit::{Artifact, CommitError, OutputSink, SourceTimes, check_space, free_space};
use crate::container::Cover;
use crate::detect::{self, Detection, MediaClass};
use crate::integrity;
use crate::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason};
use crate::preset::Speed;
use crate::probe::{self, MediaInfo, ProbeError, Stream};
use crate::text::display_path;
use crate::toolchain::{MediaKind, Toolchain};
use crate::video::mux::{self, Attachment, MuxInputs, MuxPlan};
use crate::video::plan::{VideoCaps, VideoPlan, plan_video, precheck};
use crate::video::streams::{Extracted, StreamPlan, plan_streams};
use crate::video::verify::{Expected, alpha_ssim, check_alpha, verify_output};
use crate::video::{analyze, encode as video_encode};
use crate::workspace::Workspace;

const SOURCE_METADATA: &str = "source-metadata.json";

#[derive(Debug, Clone)]
pub struct Job {
	pub source: PathBuf,
	pub speed: Speed,
	pub keep_metadata: bool,
	pub min_savings: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
	Detect,
	Probe,
	Plan,
	Queued,
	Analyze,
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

pub struct Engine {
	tools: Toolchain,
	video_slots: Semaphore,
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
	Audio { info: MediaInfo, plan: FilePlan },
	Video,
}

impl Engine {
	pub fn new(tools: Toolchain) -> Self {
		Self {
			tools,
			video_slots: Semaphore::new(1),
		}
	}

	#[must_use]
	pub fn with_video_jobs(self, jobs: usize) -> Self {
		Self {
			video_slots: Semaphore::new(jobs.max(1)),
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
		let planned = match class {
			MediaClass::Audio(info) => {
				self.require(MediaKind::Audio)?;
				on_event(JobEvent::Stage(Stage::Plan));
				match plan_file(&info, job.speed) {
					AudioPlan::Encode(plan) => Planned::Audio { info, plan },
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
				if let Err(reason) = precheck(&info, &self.video_caps()) {
					return Ok(JobOutcome::KeptOriginal { reason });
				}
				Planned::Video
			}
			MediaClass::StillImage(_) | MediaClass::AnimatedImage(_) => {
				return Ok(skipped("image support is not implemented yet"));
			}
			MediaClass::Damaged(reason) => return Err(JobError::with_log(ErrorKind::SourceDamaged(reason), probe_log)),
			MediaClass::Other(reason) => return Ok(JobOutcome::Skipped { reason }),
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
		};
		let staged = match produced.map_err(|err| disk_full(err, &work_root, cx.source_bytes))? {
			Produced::Output(staged) => staged,
			Produced::Kept(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
		};

		on_event(JobEvent::Stage(Stage::Commit));
		finish(job, sink, &work_root, &meta, before, staged)
	}

	fn require(&self, kind: MediaKind) -> Result<(), JobError> {
		let problems = self.tools.problems(kind);
		if problems.is_empty() {
			Ok(())
		} else {
			Err(ErrorKind::Toolchain(problems.join("; ")).into())
		}
	}

	fn video_caps(&self) -> VideoCaps {
		VideoCaps {
			vp9_alpha: self.tools.vp9_alpha,
			libaom: self.tools.encoders.contains("libaom-av1"),
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
		let caps = self.video_caps();

		(cx.on_event)(JobEvent::Stage(Stage::Probe));
		let (info, probe_json) = probe::probe_video(&self.tools, source, cx.cancel)
			.await
			.map_err(video_probe_error)?;
		let main = match precheck(&info, &caps) {
			Ok(main) => main,
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

		(cx.on_event)(JobEvent::Stage(Stage::Analyze));
		let declared_s = main.declared_duration().or(info.format.duration);
		let analysis = Box::pin(analyze::analyze(&self.tools, source, main, declared_s, cx.cancel)).await?;
		let video = match plan_video(&info, &frames, &analysis, cx.job.speed, &caps) {
			Ok(video) => video,
			Err(reason) => return Ok(Produced::Kept(reason)),
		};
		let streams = plan_streams(&info, cx.job.speed, cx.job.keep_metadata);

		(cx.on_event)(JobEvent::Stage(Stage::Encode));
		let (video_temp, audio_temp, audio_plan, encode_warnings) =
			Box::pin(self.encode_stage(cx, workspace, source, &video, &streams, declared_s)).await?;
		let mut warnings: Vec<String> = video.warnings.iter().chain(&streams.warnings).cloned().collect();
		warnings.extend(encode_warnings);

		(cx.on_event)(JobEvent::Stage(Stage::Mux));
		let metadata = cx.job.keep_metadata.then_some((probe_json.as_slice(), frames.first()));
		let attachments = collect_attachments(workspace, &streams.extracted, metadata, &mut warnings)?;

		let output = workspace.file("mux.mkv");
		let cmd = mux::command(
			&self.tools.ffmpeg,
			&MuxInputs {
				video: &video_temp,
				audio: audio_plan.as_ref().map(|_| audio_temp.as_path()),
				source,
				attachments: &attachments,
			},
			&MuxPlan {
				info: &info,
				video: &video,
				streams: &streams,
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
		let expected = Expected {
			video: &video,
			streams: &streams,
			source_chapters: info.chapters.len(),
			attached_files: attachments.len(),
		};
		verify_output(&self.tools, &output, &expected, cx.cancel).await?;
		if video.alpha {
			check_alpha(alpha_ssim(&self.tools, &output, source, &video, cx.cancel).await?)?;
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
	) -> Result<(PathBuf, PathBuf, Option<FilePlan>, Vec<String>), JobError> {
		let video_temp = workspace.file("video.a1.mkv");
		let audio_temp = workspace.file("audio.mka");
		let audio_plan = video_encode::audio_plan(streams);
		let video_cmd = video_encode::video_command(&self.tools.ffmpeg, source, &video_temp, video);
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

		let warnings: Vec<String> = video_encode::check_video_temp(video, &encoded, declared_s)?
			.into_iter()
			.collect();

		if let (Some(plan), Some(source_levels)) = (&audio_plan, &source_levels) {
			self.verify_audio_temp(plan, source_levels, &audio_temp, cx.cancel)
				.await?;
		}

		Ok((video_temp, audio_temp, audio_plan, warnings))
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
