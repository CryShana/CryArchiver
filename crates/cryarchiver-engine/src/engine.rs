use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::audio::plan::{AudioPlan, FilePlan, LayoutFix, TrackPlan, plan_file};
use crate::audio::{cover, encode, verify};
use crate::commit::{Artifact, CommitError, OutputSink, SourceTimes, check_space};
use crate::container::Cover;
use crate::detect::{self, Detection, MediaClass};
use crate::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason};
use crate::preset::Speed;
use crate::probe::{MediaInfo, Stream};
use crate::text::display_path;
use crate::toolchain::{MediaKind, Toolchain};
use crate::workspace::Workspace;

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
	Plan,
	Encode,
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

impl Engine {
	pub fn new(tools: Toolchain) -> Self {
		Self { tools }
	}

	pub async fn run(
		&self,
		job: &Job,
		sink: &dyn OutputSink,
		on_event: &(dyn Fn(JobEvent) + Sync),
		cancel: &CancellationToken,
	) -> JobOutcome {
		self.process(job, sink, on_event, cancel)
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
		let info = match class {
			MediaClass::Audio(info) => info,
			MediaClass::Video(_) => return Ok(skipped("video support is not implemented yet")),
			MediaClass::StillImage(_) | MediaClass::AnimatedImage(_) => {
				return Ok(skipped("image support is not implemented yet"));
			}
			MediaClass::Damaged(reason) => return Err(JobError::with_log(ErrorKind::SourceDamaged(reason), probe_log)),
			MediaClass::Other(reason) => return Ok(JobOutcome::Skipped { reason }),
		};

		let problems = self.tools.problems(MediaKind::Audio);
		if !problems.is_empty() {
			return Err(ErrorKind::Toolchain(problems.join("; ")).into());
		}

		on_event(JobEvent::Stage(Stage::Plan));
		let plan = match plan_file(&info, job.speed) {
			AudioPlan::Encode(plan) => plan,
			AudioPlan::Keep(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
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
		let staged = match self.produce_audio(&cx, &info, &plan, &workspace).await? {
			Produced::Output(staged) => staged,
			Produced::Kept(reason) => return Ok(JobOutcome::KeptOriginal { reason }),
		};

		on_event(JobEvent::Stage(Stage::Commit));
		finish(job, sink, &work_root, &meta, before, staged)
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
		for (track, (source, measured)) in plan.tracks.iter().zip(source_levels.iter().zip(&output_levels)) {
			let TrackPlan::Encode(encoded) = &track.plan else {
				continue;
			};

			let check_levels = !matches!(encoded.layout, LayoutFix::Remix(_));
			verify::compare(
				source,
				measured,
				check_levels,
				verify::lfe_index(encoded.layout.target()),
			)
			.map_err(|reason| ErrorKind::EncodeFailed(format!("verification failed: {reason}")))?;
		}

		Ok(Produced::Output(Staged {
			path: output,
			bytes,
			warnings,
		}))
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
