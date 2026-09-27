//! What runs inside a job's own task: the engine call, copies, generated files, and the commit handshake with the
//! project task, which writes the durable intent before the job renames anything (project layer spec §3.4).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use cryarchiver_engine::commit::{check_space, free_space};
use cryarchiver_engine::engine::{Encode, Job, JobEvent};
use cryarchiver_engine::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason};
use cryarchiver_engine::sink::ArchivalSink;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::owner::Message;
use crate::record::CopyReason;
use crate::run::{self, Staged};
use crate::tree::FileId;

/// How an output was made, before the project names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Made {
	Encoded { extension: String },
	Copied(CopyReason),
}

/// Where the project task wants a staged output moved.
#[derive(Debug, Clone)]
pub(crate) struct Target {
	pub(crate) path: PathBuf,
	/// The file's own earlier output at the same path, which this commit may replace.
	pub(crate) replaces: Option<PathBuf>,
}

pub(crate) enum JobMessage {
	Event(JobEvent),
	/// The staged output is ready: the project writes `Committing` durably, then answers with the target.
	Commit {
		staged: Staged,
		made: Made,
		reply: oneshot::Sender<Option<Target>>,
	},
	Placed,
	/// The target name belongs to a file the project didn't write.
	Taken,
	/// The engine kept the original; the job copies it next.
	Kept(String),
	Finished(Result<(), JobError>),
}

pub(crate) enum Task {
	Encode(Job),
	Copy(CopyReason),
}

#[derive(Clone)]
pub(crate) struct JobContext {
	pub(crate) file: FileId,
	pub(crate) source: PathBuf,
	pub(crate) size: u64,
	pub(crate) mtime_ns: i64,
	pub(crate) sink: ArchivalSink,
	pub(crate) encoder: Arc<dyn Encode>,
	pub(crate) messages: UnboundedSender<Message>,
	pub(crate) cancel: CancellationToken,
}

impl JobContext {
	fn send(&self, message: JobMessage) {
		let _ = self.messages.send(Message::Job(self.file, message));
	}

	pub(crate) async fn run(self, task: Task) {
		let result = match task {
			Task::Encode(job) => self.encode(&job).await,
			Task::Copy(reason) => self.copy(reason).await,
		};

		self.send(JobMessage::Finished(result));
	}

	async fn encode(&self, job: &Job) -> Result<(), JobError> {
		let on_event = |event: JobEvent| self.send(JobMessage::Event(event));
		match self.encoder.encode(job, &self.sink, &on_event, &self.cancel).await {
			JobOutcome::Encoded { output, .. } => {
				let extension = output
					.extension()
					.map(|extension| extension.to_string_lossy().into_owned())
					.unwrap_or_default();
				let staged = blocking(move || run::hash_staged(&output))
					.await?
					.map_err(|err| JobError::io(IoSide::Destination(&self.sink.staged_dir()), &err))?;
				self.commit(staged, Made::Encoded { extension }).await
			}
			JobOutcome::KeptOriginal { reason } => {
				self.send(JobMessage::Kept(keep_reason(&reason)));
				self.copy(CopyReason::KeptOriginal).await
			}
			JobOutcome::Skipped { .. } => self.copy(CopyReason::NotMedia).await,
			JobOutcome::Failed(err) => Err(err),
		}
	}

	async fn copy(&self, reason: CopyReason) -> Result<(), JobError> {
		let destination = self.sink.staged_dir();
		check_space(&destination, self.size)?;

		let (source, staged_path) = (self.source.clone(), self.sink.staged_path(""));
		let staged = blocking(move || run::copy_to_staged(&source, &staged_path))
			.await?
			.map_err(|err| self.copy_error(&err))?;
		if self.cancel.is_cancelled() {
			discard(&staged);
			return Err(ErrorKind::Cancelled.into());
		}

		if !run::source_unchanged(&self.source, self.size, self.mtime_ns) {
			discard(&staged);
			return Err(ErrorKind::SourceChanged.into());
		}

		self.commit(staged, Made::Copied(reason)).await
	}

	fn copy_error(&self, err: &io::Error) -> JobError {
		if err.kind() == io::ErrorKind::StorageFull {
			let need = self.size.saturating_add(self.size / 10);
			let free = free_space(&self.sink.staged_dir()).unwrap_or(0);
			return ErrorKind::InsufficientSpace { need, free }.into();
		}

		JobError::io(IoSide::Source(&self.source), err)
	}

	async fn commit(&self, staged: Staged, made: Made) -> Result<(), JobError> {
		loop {
			let (reply, answer) = oneshot::channel();
			self.send(JobMessage::Commit {
				staged: staged.clone(),
				made: made.clone(),
				reply,
			});
			let Ok(Some(target)) = answer.await else {
				discard(&staged);
				return Err(ErrorKind::Cancelled.into());
			};

			let (from, to, replaces) = (staged.path.clone(), target.path.clone(), target.replaces.clone());
			match blocking(move || run::move_into_place(&from, &to, replaces.as_deref())).await? {
				Ok(()) => {
					self.send(JobMessage::Placed);
					return Ok(());
				}
				Err(err) if err.kind() == io::ErrorKind::AlreadyExists => self.send(JobMessage::Taken),
				Err(err) => {
					discard(&staged);
					return Err(JobError::io(IoSide::Destination(&target.path), &err));
				}
			}
		}
	}
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, JobError> {
	tokio::task::spawn_blocking(work)
		.await
		.map_err(|err| ErrorKind::EncodeFailed(format!("a worker thread failed: {err}")).into())
}

fn discard(staged: &Staged) {
	let _ = std::fs::remove_file(&staged.path);
}

fn keep_reason(reason: &KeepReason) -> String {
	match reason {
		KeepReason::NotSmaller { .. } | KeepReason::PredictedNotSmaller { .. } => "not smaller".into(),
		KeepReason::AlreadyEfficient(why)
		| KeepReason::Unsupported(why)
		| KeepReason::QualityUnreachable(why)
		| KeepReason::InvalidIcc(why) => why.clone(),
		KeepReason::DecodeBomb { megapixels } => format!("the header claims {megapixels} MP"),
	}
}
