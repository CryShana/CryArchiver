//! What runs inside a job's own task: the engine call, copies, generated files, and the commit handshake with the
//! project task, which writes the durable intent before the job renames anything (project layer spec §3.4).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
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
use crate::record::{CopyReason, Hash, How, Identity, mtime_ns};
use crate::run::{self, Deletion, Expected, Staged};
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
		identity: SourceIdentity,
		reply: oneshot::Sender<Option<Target>>,
	},
	Placed,
	/// The target name belongs to a file the project didn't write.
	Taken,
	/// The engine kept the original; the job copies it next.
	Kept(String),
	/// A placeholder's source and its canonical copy's source are no longer the same file.
	NotSameFile,
	/// The job's source no longer has the size and time its record keeps: its size and time now, when they can be
	/// read.
	SourceChanged(Option<(u64, i64)>),
	/// An archived file whose content a job relies on by file identity alone, a same-file placeholder's canonical copy
	/// or a hashless take-over's holder, no longer has the size and time its record keeps.
	ArchivedChanged {
		file: RelPath,
		now: Option<(u64, i64)>,
	},
	/// The delete checks passed: the project writes `DeletePending` durably, then answers whether to go on.
	DeleteIntent {
		reply: oneshot::Sender<bool>,
	},
	/// What deleting the original did: it is gone, or another program has it open.
	Deleted(Deletion),
	/// Nothing was deleted, and why.
	DeleteRefused(Refusal),
	Finished(Result<(), JobError>),
}

/// The source's identity read with the content a job archives, and again at its commit: what a later delete checks
/// the original against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
	pub(crate) at_start: Option<Identity>,
	pub(crate) at_commit: Option<Identity>,
}

impl SourceIdentity {
	/// Whether every identity read and the one recorded, where known, name the same file.
	pub(crate) fn agrees_with(self, recorded: Option<Identity>) -> bool {
		let mut known = [self.at_start, self.at_commit, recorded].into_iter().flatten();
		let first = known.next();
		known.all(|identity| Some(identity) == first)
	}

	/// Kept only when it was read at both points: without it, the original is never deleted.
	pub(crate) fn to_record(self) -> Option<Identity> {
		self.at_start.and(self.at_commit)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
	/// The archived file no longer matches its recorded BLAKE3.
	OutputDamaged,
	/// The original isn't the file that was archived any more: its size and time now, when they can be read.
	SourceChanged(Option<(u64, i64)>),
	Kept(String),
}

pub(crate) enum Task {
	Encode(Job),
	Copy(CopyReason),
	/// Generated content: a placeholder or a rewritten reference file, with the source's timestamps.
	Write {
		content: Vec<u8>,
		made: Made,
		/// For a placeholder matched by file identity alone: the canonical copy's source, which must still be the
		/// same file, as its record describes it.
		same_file: Option<SameFile>,
	},
	Move {
		from: PathBuf,
		made: Made,
		/// For a take-over matched by file identity alone: the holder, whose source must still be the file its record
		/// describes.
		holder: Option<(RelPath, SameFile)>,
	},
	Delete(DeleteCheck),
}

/// What must hold, read back from disk, before an original is deleted (project layer spec §9).
#[derive(Debug, Clone)]
pub(crate) struct DeleteCheck {
	pub(crate) output: PathBuf,
	pub(crate) size: u64,
	pub(crate) hash: Hash,
	/// The original's identity when it was archived.
	pub(crate) identity: Identity,
	pub(crate) placeholder: Option<PlaceholderCheck>,
	pub(crate) source_root: PathBuf,
}

/// For a placeholder: the canonical copy's archived file, which holds the content.
#[derive(Debug, Clone)]
pub(crate) struct PlaceholderCheck {
	/// The link the placeholder's text must name.
	pub(crate) link: String,
	pub(crate) output: PathBuf,
	pub(crate) size: u64,
	pub(crate) hash: Hash,
	/// A placeholder matched by file identity alone: the canonical copy's source, which must still be the same file,
	/// as its record describes it.
	pub(crate) same_file: Option<SameFile>,
}

/// A canonical copy's source and the size and time its record keeps: its archived output holds that content.
#[derive(Debug, Clone)]
pub(crate) struct SameFile {
	pub(crate) path: PathBuf,
	pub(crate) size: u64,
	pub(crate) mtime_ns: i64,
}

#[derive(Clone)]
pub(crate) struct JobContext {
	pub(crate) file: FileId,
	pub(crate) source: PathBuf,
	pub(crate) size: u64,
	pub(crate) mtime_ns: i64,
	/// The source's identity when the job starts: the record's, which for a placeholder or a take-over was read with
	/// its dedup hash; a copy or an encode reads its own before it reads the content.
	pub(crate) identity: Option<Identity>,
	pub(crate) sink: ArchivalSink,
	pub(crate) encoder: Arc<dyn Encode>,
	pub(crate) messages: UnboundedSender<Message>,
	pub(crate) cancel: CancellationToken,
}

impl JobContext {
	fn send(&self, message: JobMessage) {
		let _ = self.messages.send(Message::Job(self.file, message));
	}

	pub(crate) async fn run(mut self, task: Task) {
		if matches!(task, Task::Encode(_) | Task::Copy(_)) {
			self.identity = self.source_identity().await;
		}

		let result = match task {
			Task::Encode(job) => self.encode(&job).await,
			Task::Copy(reason) => self.copy(reason).await,
			Task::Write {
				content,
				made,
				same_file,
			} => self.write(content, made, same_file).await,
			Task::Move { from, made, holder } => self.take_over(from, made, holder).await,
			Task::Delete(check) => self.delete(check).await,
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

				// The engine checks the source only against what it found itself; the record, with its dedup hashes,
				// stands for what the project saw.
				if !run::source_unchanged(&self.source, self.size, self.mtime_ns) {
					discard(&staged);
					return Err(self.source_changed().await);
				}

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
			return Err(self.source_changed().await);
		}

		self.commit(staged, Made::Copied(reason)).await
	}

	async fn write(&self, content: Vec<u8>, made: Made, same_file: Option<SameFile>) -> Result<(), JobError> {
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
			return Err(self.source_changed().await);
		}

		if let Some(canonical) = same_file {
			let (source, path) = (self.source.clone(), canonical.path.clone());
			let same = blocking(move || matches!((file_id(&source), file_id(&path)), (Ok(a), Ok(b)) if a == b)).await?;
			if !same {
				discard(&staged);
				self.send(JobMessage::NotSameFile);
				return Err(ErrorKind::Cancelled.into());
			}

			// The same file now; the canonical copy's archived content is only this file's content while its record
			// still describes it.
			if !run::source_unchanged(&canonical.path, canonical.size, canonical.mtime_ns) {
				discard(&staged);
				let path = canonical.path;
				let now = blocking(move || size_and_time(&path)).await?;
				if let Made::Placeholder { canonical, .. } = &made {
					self.send(JobMessage::ArchivedChanged {
						file: canonical.clone(),
						now,
					});
				}
				return Err(ErrorKind::Cancelled.into());
			}
		}

		self.commit(staged, made).await
	}

	async fn take_over(&self, from: PathBuf, made: Made, holder: Option<(RelPath, SameFile)>) -> Result<(), JobError> {
		if !run::source_unchanged(&self.source, self.size, self.mtime_ns) {
			return Err(self.source_changed().await);
		}

		// The holder's archived content is this file's only while the holder's record still describes their source.
		if let Some((key, recorded)) = holder
			&& !run::source_unchanged(&recorded.path, recorded.size, recorded.mtime_ns)
		{
			let path = recorded.path;
			let now = blocking(move || size_and_time(&path)).await?;
			self.send(JobMessage::ArchivedChanged { file: key, now });
			return Err(ErrorKind::Cancelled.into());
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

	/// Checks the archive and the original, asks the project for its durable intent, and only then deletes. Nothing is
	/// retried here: the project decides what comes next.
	async fn delete(&self, check: DeleteCheck) -> Result<(), JobError> {
		let (source, size, mtime_ns) = (self.source.clone(), self.size, self.mtime_ns);
		let verify = check.clone();
		let verdict = blocking(move || verify_before_delete(&verify, &source, size, mtime_ns)).await??;
		if let Err(refusal) = verdict {
			self.send(JobMessage::DeleteRefused(refusal));
			return Ok(());
		}

		let (reply, answer) = oneshot::channel();
		self.send(JobMessage::DeleteIntent { reply });
		if !answer.await.unwrap_or(false) || self.cancel.is_cancelled() {
			return Err(ErrorKind::Cancelled.into());
		}

		let (source, root) = (self.source.clone(), check.source_root);
		let expected = Expected {
			size: self.size,
			mtime_ns: self.mtime_ns,
			identity: Some(check.identity),
		};
		let deletion = blocking(move || run::delete_original(&source, &root, &expected))
			.await?
			.map_err(|err| JobError::io(IoSide::Source(&self.source), &err))?;

		let message = match deletion {
			Deletion::Changed => {
				let source = self.source.clone();
				let now = blocking(move || size_and_time(&source)).await?;
				JobMessage::DeleteRefused(Refusal::SourceChanged(now))
			}
			Deletion::Refused(reason) => JobMessage::DeleteRefused(Refusal::Kept(reason.into())),
			deletion => JobMessage::Deleted(deletion),
		};
		self.send(message);
		Ok(())
	}

	/// The source no longer has the size and time its record keeps: the project archives it again as it is now.
	async fn source_changed(&self) -> JobError {
		let source = self.source.clone();
		let now = blocking(move || size_and_time(&source)).await.ok().flatten();
		self.send(JobMessage::SourceChanged(now));

		ErrorKind::SourceChanged.into()
	}

	async fn source_identity(&self) -> Option<Identity> {
		let source = self.source.clone();
		blocking(move || file_id(&source).ok().map(Identity::from))
			.await
			.ok()
			.flatten()
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

		let identity = SourceIdentity {
			at_start: self.identity,
			at_commit: self.source_identity().await,
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
				identity,
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

/// The read-back and source checks of spec §9, cheapest first. A source that can't be reached is an error: it proves
/// neither a change nor that the original is gone. So is an archived file that can't be read back: only its content
/// can prove it damaged.
fn verify_before_delete(
	check: &DeleteCheck,
	source: &Path,
	size: u64,
	mtime_ns: i64,
) -> Result<Result<(), Refusal>, JobError> {
	let read_back = |path: &Path, size, hash| read_back(path, size, hash).map_err(|err| destination_error(path, &err));

	let archived = source_as_archived(source, size, mtime_ns).map_err(|err| JobError::io(IoSide::Source(source), &err));
	if let Err(refusal) = archived? {
		return Ok(Err(refusal));
	}

	if !read_back(&check.output, check.size, check.hash)? {
		return Ok(Err(Refusal::OutputDamaged));
	}

	let Some(placeholder) = &check.placeholder else {
		return Ok(Ok(()));
	};

	let text = match fs::read_to_string(&check.output) {
		Ok(text) => Some(text),
		Err(err) if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData) => None,
		Err(err) => return Err(destination_error(&check.output, &err)),
	};
	if !text.is_some_and(|text| run::placeholder_links_to(&text, &placeholder.link)) {
		return Ok(Err(Refusal::Kept(
			"the placeholder no longer names the archived copy".into(),
		)));
	}

	if !read_back(&placeholder.output, placeholder.size, placeholder.hash)? {
		return Ok(Err(Refusal::Kept(
			"the archived copy it points to doesn't match its BLAKE3".into(),
		)));
	}

	if let Some(canonical) = &placeholder.same_file {
		let same = matches!((file_id(source), file_id(&canonical.path)), (Ok(a), Ok(b)) if a == b);
		if !same {
			return Ok(Err(Refusal::Kept(
				"it is no longer the same file as the archived copy's source".into(),
			)));
		}

		if !run::source_unchanged(&canonical.path, canonical.size, canonical.mtime_ns) {
			return Ok(Err(Refusal::Kept(
				"the archived copy's source changed since it was archived".into(),
			)));
		}
	}

	Ok(Ok(()))
}

/// A missing archived file is as good as a damaged one; any other error reading it proves nothing.
fn read_back(path: &Path, size: u64, hash: Hash) -> io::Result<bool> {
	match run::read_back(path, size, hash) {
		Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
		result => result,
	}
}

fn destination_error(path: &Path, err: &io::Error) -> JobError {
	JobError::io(IoSide::Destination(path), err)
}

/// The original has the size and time it was archived with, or it changed, or it is provably gone: its folder is
/// there and the file isn't.
fn source_as_archived(source: &Path, size: u64, modified_ns: i64) -> io::Result<Result<(), Refusal>> {
	match fs::metadata(source) {
		Ok(meta) => {
			let now = meta.modified().ok().map(|modified| (meta.len(), mtime_ns(modified)));
			let unchanged = meta.is_file() && now == Some((size, modified_ns));
			Ok(if unchanged {
				Ok(())
			} else {
				Err(Refusal::SourceChanged(now))
			})
		}
		Err(err) if err.kind() == io::ErrorKind::NotFound && source.parent().is_some_and(Path::is_dir) => {
			Ok(Err(Refusal::SourceChanged(None)))
		}
		Err(err) => Err(err),
	}
}

fn size_and_time(path: &Path) -> Option<(u64, i64)> {
	let meta = fs::metadata(path).ok()?;
	Some((meta.len(), mtime_ns(meta.modified().ok()?)))
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

	use cryarchiver_engine::commit::{Artifact, OutputSink};
	use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

	use super::*;

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

	/// Rewrites the source in place, as a tagger would just before the encode reads it, then encodes what it holds.
	struct RewritingEncoder(Vec<u8>);

	impl Encode for RewritingEncoder {
		fn encode<'a>(
			&'a self,
			job: &'a Job,
			sink: &'a dyn OutputSink,
			_on_event: &'a (dyn Fn(JobEvent) + Sync),
			_cancel: &'a CancellationToken,
		) -> Pin<Box<dyn Future<Output = JobOutcome> + Send + 'a>> {
			Box::pin(async move {
				fs::write(&job.source, &self.0).unwrap();

				let work_root = sink.work_root(&job.source);
				fs::create_dir_all(&work_root).unwrap();
				let work = work_root.join("encoded.avif");
				fs::write(&work, fs::read(&job.source).unwrap()).unwrap();
				let output = sink
					.commit(&Artifact {
						path: work,
						source: job.source.clone(),
						times: None,
					})
					.unwrap();

				JobOutcome::Encoded {
					out_bytes: fs::metadata(&output).unwrap().len(),
					src_bytes: self.0.len() as u64,
					output,
					warnings: Vec::new(),
				}
			})
		}
	}

	/// A job for `source` as the project last saw it, run to its end with every commit refused; returns what it sent.
	fn run_job(source: &Path, destination: &Path, seen: &[u8], task: Task) -> Vec<JobMessage> {
		run_job_with(Arc::new(NoEncoder), source, destination, seen, task)
	}

	fn run_job_with(
		encoder: Arc<dyn Encode>,
		source: &Path,
		destination: &Path,
		seen: &[u8],
		task: Task,
	) -> Vec<JobMessage> {
		let meta = fs::metadata(source).unwrap();
		let (messages, mut inbox): (_, UnboundedReceiver<Message>) = unbounded_channel();
		let context = JobContext {
			file: FileId(0),
			source: source.to_path_buf(),
			size: seen.len() as u64,
			mtime_ns: mtime_ns(meta.modified().unwrap()),
			identity: None,
			sink: ArchivalSink::new(destination),
			encoder,
			messages,
			cancel: CancellationToken::new(),
		};

		let runtime = tokio::runtime::Runtime::new().unwrap();
		runtime.block_on(async move {
			tokio::spawn(context.run(task));

			let mut sent = Vec::new();
			while let Some(Message::Job(_, message)) = inbox.recv().await {
				let message = match message {
					JobMessage::Commit {
						staged,
						made,
						identity,
						reply,
					} => {
						let _ = reply.send(None);
						let (unanswered, _) = oneshot::channel();
						JobMessage::Commit {
							staged,
							made,
							identity,
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

	fn committed(sent: &[JobMessage]) -> bool {
		sent.iter().any(|message| matches!(message, JobMessage::Commit { .. }))
	}

	/// The file at `path` as a record would describe it now.
	fn as_recorded(path: &Path) -> SameFile {
		let meta = fs::metadata(path).unwrap();
		SameFile {
			path: path.to_path_buf(),
			size: meta.len(),
			mtime_ns: mtime_ns(meta.modified().unwrap()),
		}
	}

	fn delete_check(output: &Path, content: &[u8]) -> DeleteCheck {
		DeleteCheck {
			output: output.to_path_buf(),
			size: content.len() as u64,
			hash: Hash(*blake3::hash(content).as_bytes()),
			identity: Identity { volume: 0, index: 0 },
			placeholder: None,
			source_root: PathBuf::new(),
		}
	}

	fn placeholder(same_file: Option<SameFile>) -> Task {
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
	fn an_identity_is_kept_only_when_every_read_names_the_same_file() {
		let (a, b) = (Identity { volume: 1, index: 1 }, Identity { volume: 1, index: 2 });
		let read = |at_start, at_commit| SourceIdentity { at_start, at_commit };

		assert!(read(Some(a), Some(a)).agrees_with(Some(a)));
		assert_eq!(read(Some(a), Some(a)).to_record(), Some(a));
		assert!(!read(Some(a), Some(b)).agrees_with(None), "replaced during the job");
		assert!(!read(Some(b), Some(b)).agrees_with(Some(a)), "replaced after the hash");
		assert!(!read(None, Some(b)).agrees_with(Some(a)));

		assert!(read(None, Some(a)).agrees_with(Some(a)));
		assert_eq!(read(None, Some(a)).to_record(), None, "not read at the start");
		assert_eq!(read(Some(a), None).to_record(), None, "not read at the commit");
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
			holder: None,
		};

		let sent = run_job(&source, &destination, b"seen, then more", task);

		assert!(failed_with(&sent, &ErrorKind::SourceChanged));
		assert!(!sent.iter().any(|message| matches!(message, JobMessage::Commit { .. })));
		assert_eq!(fs::read(&held).unwrap(), b"archived");
	}

	#[test]
	fn a_take_over_by_file_identity_needs_the_holders_source_as_its_record_describes_it() {
		let dir = tempfile::tempdir().unwrap();
		let holder = dir.path().join("Deep").join("a.bin");
		fs::create_dir_all(holder.parent().unwrap()).unwrap();
		fs::write(&holder, b"seen").unwrap();
		let source = dir.path().join("a.bin");
		fs::hard_link(&holder, &source).unwrap();
		let destination = dir.path().join("archive");
		let held = destination.join("Deep").join("a.bin");
		fs::create_dir_all(held.parent().unwrap()).unwrap();
		fs::write(&held, b"archived").unwrap();
		let take_over = |recorded: SameFile| Task::Move {
			from: held.clone(),
			made: Made::Moved {
				how: How::Copied(CopyReason::NotMedia),
				hash: None,
				extension: "bin".into(),
			},
			holder: Some((RelPath::from_bytes(b"Deep/a.bin".to_vec()), recorded)),
		};

		let recorded = as_recorded(&holder);
		let stale = SameFile {
			mtime_ns: recorded.mtime_ns - 1,
			..recorded.clone()
		};
		let sent = run_job(&source, &destination, b"seen", take_over(stale));

		let now = (recorded.size, recorded.mtime_ns);
		assert!(sent.iter().any(|message| matches!(
			message,
			JobMessage::ArchivedChanged { file, now: Some(found) } if file.display() == "Deep/a.bin" && *found == now
		)));
		assert!(failed_with(&sent, &ErrorKind::Cancelled));
		assert!(!committed(&sent));
		assert_eq!(fs::read(&held).unwrap(), b"archived");

		let sent = run_job(&source, &destination, b"seen", take_over(recorded));
		assert!(committed(&sent));
	}

	#[test]
	fn a_placeholder_of_the_same_file_is_written_only_while_it_still_is() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("b.bin");
		let other = dir.path().join("a.bin");
		fs::write(&source, b"seen").unwrap();
		fs::write(&other, b"seen").unwrap();
		let destination = dir.path().join("archive");

		let sent = run_job(&source, &destination, b"seen", placeholder(Some(as_recorded(&other))));

		assert!(sent.iter().any(|message| matches!(message, JobMessage::NotSameFile)));
		assert!(failed_with(&sent, &ErrorKind::Cancelled));
		assert!(!sent.iter().any(|message| matches!(message, JobMessage::Commit { .. })));
		assert_eq!(staged_files(&destination), 0);
	}

	#[test]
	fn a_placeholder_of_the_same_file_is_written_only_while_the_canonical_copys_record_describes_it() {
		let dir = tempfile::tempdir().unwrap();
		let canonical = dir.path().join("a.bin");
		let source = dir.path().join("b.bin");
		fs::write(&canonical, b"seen").unwrap();
		fs::hard_link(&canonical, &source).unwrap();
		let destination = dir.path().join("archive");

		let recorded = as_recorded(&canonical);
		let stale = SameFile {
			mtime_ns: recorded.mtime_ns - 1,
			..recorded.clone()
		};
		let sent = run_job(&source, &destination, b"seen", placeholder(Some(stale)));

		let now = (recorded.size, recorded.mtime_ns);
		assert!(sent.iter().any(|message| matches!(
			message,
			JobMessage::ArchivedChanged { file, now: Some(found) } if file.display() == "a.bin" && *found == now
		)));
		assert!(failed_with(&sent, &ErrorKind::Cancelled));
		assert!(!committed(&sent));
		assert_eq!(staged_files(&destination), 0);

		let sent = run_job(&source, &destination, b"seen", placeholder(Some(recorded)));
		assert!(committed(&sent));
	}

	#[test]
	fn an_encode_of_a_source_that_changed_after_the_project_saw_it_is_never_committed() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.flac");
		fs::write(&source, b"tags v1").unwrap();
		let destination = dir.path().join("archive");
		let encoder = Arc::new(RewritingEncoder(b"tags v2, longer".to_vec()));
		let job = Job {
			source: source.clone(),
			speed: cryarchiver_engine::preset::Speed::Fast,
			keep_metadata: true,
			drop_aux: true,
			min_savings: 0.0,
		};

		let sent = run_job_with(encoder, &source, &destination, b"tags v1", Task::Encode(job));

		let now = (15, mtime_ns(fs::metadata(&source).unwrap().modified().unwrap()));
		assert!(
			sent.iter()
				.any(|message| matches!(message, JobMessage::SourceChanged(Some(found)) if *found == now))
		);
		assert!(failed_with(&sent, &ErrorKind::SourceChanged));
		assert!(!committed(&sent));
		assert_eq!(staged_files(&destination), 0);
	}

	#[test]
	fn an_archived_file_that_cant_be_read_back_is_an_error_never_damage() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("a.txt");
		fs::write(&source, b"notes").unwrap();
		let recorded = as_recorded(&source);
		let output = dir.path().join("archived.txt");
		fs::write(&output, b"notes").unwrap();
		let verify = |check: &DeleteCheck| verify_before_delete(check, &source, recorded.size, recorded.mtime_ns);

		assert!(matches!(verify(&delete_check(&output, b"notes")), Ok(Ok(()))));
		assert!(matches!(
			verify(&delete_check(&output, b"NOTES")),
			Ok(Err(Refusal::OutputDamaged))
		));
		assert!(matches!(
			verify(&delete_check(&dir.path().join("gone.txt"), b"notes")),
			Ok(Err(Refusal::OutputDamaged))
		));

		#[cfg(windows)]
		{
			use std::os::windows::fs::OpenOptionsExt;

			let exclusive = fs::File::options().read(true).share_mode(0).open(&output).unwrap();
			assert!(verify(&delete_check(&output, b"notes")).is_err());
			drop(exclusive);
		}

		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;

			fs::set_permissions(&output, fs::Permissions::from_mode(0o000)).unwrap();
			// SAFETY: geteuid only reads the process's credentials.
			if unsafe { libc::geteuid() } != 0 {
				assert!(verify(&delete_check(&output, b"notes")).is_err());
			}
			fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).unwrap();
		}
	}

	#[test]
	fn a_same_file_placeholders_original_is_kept_once_its_canonical_copys_source_changed() {
		let dir = tempfile::tempdir().unwrap();
		let canonical = dir.path().join("a.bin");
		let source = dir.path().join("b.bin");
		fs::write(&canonical, b"content").unwrap();
		fs::hard_link(&canonical, &source).unwrap();
		let recorded = as_recorded(&source);
		let text = run::placeholder_text("a.bin", "b.bin", recorded.size, None);
		let placeholder = dir.path().join("placeholder_b.bin.txt");
		fs::write(&placeholder, &text).unwrap();
		let output = dir.path().join("a.bin.archived");
		fs::write(&output, b"content").unwrap();
		let check = |same_file: SameFile| DeleteCheck {
			placeholder: Some(PlaceholderCheck {
				link: "a.bin".into(),
				output: output.clone(),
				size: 7,
				hash: Hash(*blake3::hash(b"content").as_bytes()),
				same_file: Some(same_file),
			}),
			..delete_check(&placeholder, text.as_bytes())
		};
		let verify = |check: &DeleteCheck| verify_before_delete(check, &source, recorded.size, recorded.mtime_ns);

		assert!(matches!(verify(&check(as_recorded(&canonical))), Ok(Ok(()))));

		let stale = SameFile {
			mtime_ns: recorded.mtime_ns - 1,
			..as_recorded(&canonical)
		};
		assert!(matches!(
			verify(&check(stale)),
			Ok(Err(Refusal::Kept(reason))) if reason == "the archived copy's source changed since it was archived"
		));
	}
}
