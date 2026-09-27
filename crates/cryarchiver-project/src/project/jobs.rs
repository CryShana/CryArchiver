//! What runs inside a job's own task: the engine call, copies, generated files, and the commit handshake with the
//! project task, which writes the durable intent before the job renames anything (project layer spec §3.4).

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use cryarchiver_engine::commit::{SourceTimes, check_space, file_id, free_space};
use cryarchiver_engine::engine::{Encode, Job, JobEvent};
use cryarchiver_engine::outcome::{ErrorKind, IoSide, JobError, JobOutcome, KeepReason};
use cryarchiver_engine::sink::ArchivalSink;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::owner::Message;
use crate::keys::RelPath;
use crate::record::{CopyReason, Hash, How};
use crate::run::{self, Staged};
use crate::tree::FileId;

/// How an output was made, before the project names it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Made {
	Encoded {
		extension: String,
	},
	Copied(CopyReason),
	Placeholder {
		canonical: RelPath,
		/// The canonical copy's output the placeholder's text links to.
		target: RelPath,
	},
	/// Another member's archived output, taken over by rename.
	Moved {
		how: How,
		hash: Option<Hash>,
		extension: String,
	},
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
	/// A placeholder's source and its canonical copy's source are no longer the same file.
	NotSameFile,
	Finished(Result<(), JobError>),
}

pub(crate) enum Task {
	Encode(Job),
	Copy(CopyReason),
	/// Generated content: a placeholder or a rewritten reference file, with the source's timestamps.
	Write {
		content: Vec<u8>,
		made: Made,
		/// For a placeholder matched by file identity alone: the canonical copy's source, which must still be the
		/// same file.
		same_file: Option<PathBuf>,
	},
	Move {
		from: PathBuf,
		made: Made,
	},
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
			Task::Write {
				content,
				made,
				same_file,
			} => self.write(content, made, same_file).await,
			Task::Move { from, made } => self.take_over(from, made).await,
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

	async fn write(&self, content: Vec<u8>, made: Made, same_file: Option<PathBuf>) -> Result<(), JobError> {
		let (source, staged_path) = (self.source.clone(), self.sink.staged_path(""));
		let staged = blocking(move || {
			let times = fs::metadata(&source)
				.and_then(|meta| SourceTimes::from_metadata(&meta))
				.ok();
			run::write_staged(&staged_path, &content, times)
		})
		.await?
		.map_err(|err| JobError::io(IoSide::Destination(&self.sink.staged_dir()), &err))?;

		// The content stands for the source as the project last saw it: its hashes, or the text the ready check read.
		if !run::source_unchanged(&self.source, self.size, self.mtime_ns) {
			discard(&staged);
			return Err(ErrorKind::SourceChanged.into());
		}

		if let Some(canonical) = same_file {
			let source = self.source.clone();
			let same =
				blocking(move || matches!((file_id(&source), file_id(&canonical)), (Ok(a), Ok(b)) if a == b)).await?;
			if !same {
				discard(&staged);
				self.send(JobMessage::NotSameFile);
				return Err(ErrorKind::Cancelled.into());
			}
		}

		self.commit(staged, made).await
	}

	async fn take_over(&self, from: PathBuf, made: Made) -> Result<(), JobError> {
		if !run::source_unchanged(&self.source, self.size, self.mtime_ns) {
			return Err(ErrorKind::SourceChanged.into());
		}

		let size = fs::metadata(&from)
			.map_err(|err| JobError::io(IoSide::Destination(&from), &err))?
			.len();
		let hash = match &made {
			Made::Moved { hash: Some(hash), .. } => *hash,
			_ => Hash([0; 32]),
		};

		self.commit(Staged { path: from, size, hash }, made).await
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
		// A take-over's "staged" file is another member's archived output: it must stay where it is.
		let discard_unplaced = |staged: &Staged| {
			if !matches!(made, Made::Moved { .. }) {
				discard(staged);
			}
		};

		loop {
			if self.cancel.is_cancelled() {
				discard_unplaced(&staged);
				return Err(ErrorKind::Cancelled.into());
			}

			let (reply, answer) = oneshot::channel();
			self.send(JobMessage::Commit {
				staged: staged.clone(),
				made: made.clone(),
				reply,
			});
			let Ok(Some(target)) = answer.await else {
				discard_unplaced(&staged);
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
					discard_unplaced(&staged);
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

#[cfg(test)]
mod tests {
	use std::path::Path;
	use std::pin::Pin;

	use cryarchiver_engine::commit::OutputSink;
	use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

	use super::*;
	use crate::record::mtime_ns;

	struct NoEncoder;

	impl Encode for NoEncoder {
		fn encode<'a>(
			&'a self,
			_job: &'a Job,
			_sink: &'a dyn OutputSink,
			_on_event: &'a (dyn Fn(JobEvent) + Sync),
			_cancel: &'a CancellationToken,
		) -> Pin<Box<dyn Future<Output = JobOutcome> + Send + 'a>> {
			unreachable!("these jobs never encode")
		}
	}

	/// A job for `source` as the project last saw it, run to its end with every commit refused; returns what it sent.
	fn run_job(source: &Path, destination: &Path, seen: &[u8], task: Task) -> Vec<JobMessage> {
		let meta = fs::metadata(source).unwrap();
		let (messages, mut inbox): (_, UnboundedReceiver<Message>) = unbounded_channel();
		let context = JobContext {
			file: FileId(0),
			source: source.to_path_buf(),
			size: seen.len() as u64,
			mtime_ns: mtime_ns(meta.modified().unwrap()),
			sink: ArchivalSink::new(destination),
			encoder: Arc::new(NoEncoder),
			messages,
			cancel: CancellationToken::new(),
		};

		let runtime = tokio::runtime::Runtime::new().unwrap();
		runtime.block_on(async move {
			tokio::spawn(context.run(task));

			let mut sent = Vec::new();
			while let Some(Message::Job(_, message)) = inbox.recv().await {
				let message = match message {
					JobMessage::Commit { staged, made, reply } => {
						let _ = reply.send(None);
						let (unanswered, _) = oneshot::channel();
						JobMessage::Commit {
							staged,
							made,
							reply: unanswered,
						}
					}
					message => message,
				};

				let finished = matches!(message, JobMessage::Finished(_));
				sent.push(message);
				if finished {
					break;
				}
			}
			sent
		})
	}

	fn failed_with(sent: &[JobMessage], kind: &ErrorKind) -> bool {
		matches!(sent.last(), Some(JobMessage::Finished(Err(err))) if err.kind == *kind)
	}

	fn staged_files(destination: &Path) -> usize {
		fs::read_dir(ArchivalSink::new(destination).staged_dir()).map_or(0, Iterator::count)
	}

	fn placeholder(same_file: Option<PathBuf>) -> Task {
		Task::Write {
			content: b"placeholder".to_vec(),
			made: Made::Placeholder {
				canonical: RelPath::from_bytes(b"a.bin".to_vec()),
				target: RelPath::from_bytes(b"a.bin".to_vec()),
			},
			same_file,
		}
	}

	#[test]
	fn a_placeholder_is_never_committed_for_a_source_that_changed() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("b.bin");
		fs::write(&source, b"seen").unwrap();
		let destination = dir.path().join("archive");

		let sent = run_job(&source, &destination, b"seen, then more", placeholder(None));

		assert!(failed_with(&sent, &ErrorKind::SourceChanged));
		assert!(!sent.iter().any(|message| matches!(message, JobMessage::Commit { .. })));
		assert_eq!(staged_files(&destination), 0);
	}

	#[test]
	fn an_output_is_never_taken_over_for_a_source_that_changed() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.bin");
		fs::write(&source, b"seen").unwrap();
		let destination = dir.path().join("archive");
		let held = dir.path().join("archive").join("Deep").join("a.bin");
		fs::create_dir_all(held.parent().unwrap()).unwrap();
		fs::write(&held, b"archived").unwrap();
		let task = Task::Move {
			from: held.clone(),
			made: Made::Moved {
				how: How::Copied(CopyReason::NotMedia),
				hash: None,
				extension: "bin".into(),
			},
		};

		let sent = run_job(&source, &destination, b"seen, then more", task);

		assert!(failed_with(&sent, &ErrorKind::SourceChanged));
		assert!(!sent.iter().any(|message| matches!(message, JobMessage::Commit { .. })));
		assert_eq!(fs::read(&held).unwrap(), b"archived");
	}

	#[test]
	fn a_placeholder_of_the_same_file_is_written_only_while_it_still_is() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("b.bin");
		let other = dir.path().join("a.bin");
		fs::write(&source, b"seen").unwrap();
		fs::write(&other, b"seen").unwrap();
		let destination = dir.path().join("archive");

		let sent = run_job(&source, &destination, b"seen", placeholder(Some(other)));

		assert!(sent.iter().any(|message| matches!(message, JobMessage::NotSameFile)));
		assert!(failed_with(&sent, &ErrorKind::Cancelled));
		assert!(!sent.iter().any(|message| matches!(message, JobMessage::Commit { .. })));
		assert_eq!(staged_files(&destination), 0);
	}
}
