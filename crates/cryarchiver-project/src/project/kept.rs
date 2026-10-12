//! Files whose original was kept, archived again on the user's request: the engine may do better now. Until the new
//! output is in place, the kept copy stays the file's archive: a retry that fails, or whose source vanishes first, falls
//! back to it.

use super::Target;
use super::owner::Owner;
use crate::record::{CopyReason, FileState, How, Output};
use crate::tree::{FileId, FolderId};
use crate::view::{LogKind, NoticeLevel};

impl Owner {
	pub(super) fn retry_kept(&mut self, target: Target) {
		let candidates = match target {
			Target::File(file) if self.tree.has_file(file) => vec![file],
			Target::Folder(folder) if self.tree.has_folder(folder) => self.files_below(folder),
			Target::File(_) | Target::Folder(_) => Vec::new(),
		};
		let (busy, kept): (Vec<FileId>, Vec<FileId>) = candidates
			.into_iter()
			.filter(|&file| self.is_kept(file))
			.partition(|&file| self.busy(file));

		if kept.is_empty() {
			let text = match (target, busy.is_empty()) {
				(_, true) => "nothing kept to retry here",
				(Target::File(_), false) => "it's busy; try again once it finishes",
				(Target::Folder(_), false) => "its kept files are busy; try again once they finish",
			};
			self.notice(NoticeLevel::Info, text.into());
			return;
		}

		for &file in &kept {
			self.archive_kept_again(file);
		}

		let message = match target {
			Target::File(file) => format!("{}: retry chosen", self.tree.file(file).key.display()),
			Target::Folder(folder) => retrying(kept.len(), busy.len(), &self.tree.folder(folder).key.display()),
		};
		self.log(LogKind::Action, message);
	}

	fn files_below(&self, folder: FolderId) -> Vec<FileId> {
		if self.tree.totals(folder).kept_done == 0 {
			return Vec::new();
		}

		let mut files = Vec::new();
		let mut folders = vec![folder];
		while let Some(current) = folders.pop() {
			let entry = self.tree.folder(current);
			files.extend(&entry.files);
			folders.extend(&entry.children);
		}
		files
	}

	fn is_kept(&self, file: FileId) -> bool {
		let entry = self.tree.file(file);
		let kept = matches!(
			&entry.record.state,
			FileState::Done(output) if output.how == How::Copied(CopyReason::KeptOriginal)
		);

		kept && !entry.removed
	}

	/// A job runs for it, or a take-over is about to move its copy.
	fn busy(&self, file: FileId) -> bool {
		self.pipeline.jobs.contains_key(&file) || self.gives_output_to_a_take_over(file)
	}

	/// As a duplicate whose canonical copy changed is archived again: a placeholder pointing at the kept copy adopts
	/// it, and otherwise the new output replaces the kept copy only once it is in place.
	fn archive_kept_again(&mut self, file: FileId) {
		let FileState::Done(output) = self.tree.file(file).record.state.clone() else {
			return;
		};

		let adopted = self.transfer_dependents(file, Some(&output));
		self.remember_retried(file, output.path.clone());
		let mut record = self.tree.file(file).record.clone();
		record.replaces = (!adopted).then(|| output.path.clone());
		record.retrying = (!adopted).then_some(output);
		self.tree.set_record(file, record);

		self.pipeline.chosen_copies.remove(&file);
		self.set_state(file, FileState::Queued);
		self.unqueue(file);
		self.enqueue(file);
	}

	/// The kept copy of a retry that hasn't started to commit: the file's archive still, if the retry places nothing.
	pub(super) fn pending_retry(&self, file: FileId) -> Option<Output> {
		let record = &self.tree.file(file).record;
		let pending = matches!(
			record.state,
			FileState::Discovered | FileState::Ready | FileState::Queued | FileState::InProgress { .. }
		);

		record.retrying.clone().filter(|_| pending)
	}

	/// The source vanished before the retry placed anything: the kept copy stays its archive, as for any archived file
	/// whose source is gone.
	pub(super) fn retry_source_gone(&mut self, file: FileId, kept: Output) {
		self.withdraw(file);
		self.give_back_kept(file, &kept);
		self.set_state(file, FileState::SourceGone(kept));
		self.join_dedup(file);
		self.member_changed(file);
	}

	/// A retry that fails leaves the file as it was, with a note saying why.
	pub(super) fn retry_failed(&mut self, file: FileId, kept: Output, reason: &str) {
		self.pipeline.retries.remove(&file);
		self.pipeline.retry_threads.remove(&file);
		self.give_back_kept(file, &kept);
		self.set_state(file, FileState::Done(kept));
		self.note(
			file,
			format!("the retry failed: {reason}; the kept copy stays its archive"),
		);

		self.file_settled(file);
		self.queue_delete(file);
	}

	fn give_back_kept(&mut self, file: FileId, kept: &Output) {
		self.forget_retried(file);
		let mut record = self.tree.file(file).record.clone();
		record.replaces = None;
		record.retrying = None;
		self.tree.set_record(file, record);
		self.claim(file, &kept.path);
	}
}

fn retrying(count: usize, busy: usize, folder: &str) -> String {
	let files = if count == 1 { "file" } else { "files" };
	let place = if folder.is_empty() {
		String::new()
	} else {
		format!(" in {folder}")
	};
	let left = match busy {
		0 => String::new(),
		1 => "; 1 busy file left as it is".to_owned(),
		busy => format!("; {busy} busy files left as they are"),
	};
	format!("retrying {count} kept {files}{place}{left}")
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::PathBuf;
	use std::sync::mpsc::Receiver;

	use cryarchiver_engine::outcome::ErrorKind;
	use tokio::sync::oneshot;

	use super::*;
	use crate::keys::RelPath;
	use crate::project::jobs::{JobMessage, Made, SourceIdentity, Target as Placement};
	use crate::project::pipeline::tests::running_job;
	use crate::project::scanning::tests::{key, owner};
	use crate::record::{FileRecord, Hash, IssueKind, Output};
	use crate::run::Staged;
	use crate::schedule::Work;
	use crate::view::Update;

	const SIZE: u64 = 100_000;

	struct Fixture {
		_dir: tempfile::TempDir,
		owner: Owner,
		updates: Receiver<Update>,
	}

	impl Fixture {
		fn new() -> Self {
			let dir = tempfile::tempdir().unwrap();
			let source = dir.path().join("source");
			fs::create_dir_all(&source).unwrap();
			let (owner, updates) = owner(&source, &dir.path().join("archive"));
			Self {
				_dir: dir,
				owner,
				updates,
			}
		}

		fn folder(&mut self, path: &str) -> FolderId {
			self.owner.tree.ensure_folder(&key(path))
		}

		/// A file at `path` in the state, its output, if any, on disk in the destination.
		fn add(&mut self, path: &str, state: FileState) -> FileId {
			let path = key(path);
			let folder = path
				.parent()
				.map_or(FolderId::ROOT, |parent| self.owner.tree.ensure_folder(&parent));
			let output = state.output().cloned();
			let file = self.owner.tree.add_file(
				folder,
				&path.name(),
				FileRecord {
					state,
					..FileRecord::discovered(SIZE, 1)
				},
			);
			self.owner.note_placeholder(file);

			if let Some(output) = output {
				let archived = self.owner.destination_path(&output.path);
				fs::create_dir_all(archived.parent().unwrap()).unwrap();
				fs::write(&archived, b"kept copy").unwrap();
				self.owner.claim(file, &output.path);
			}
			file
		}

		fn state(&self, file: FileId) -> &FileState {
			&self.owner.tree.file(file).record.state
		}

		fn replaces(&self, file: FileId) -> Option<&RelPath> {
			self.owner.tree.file(file).record.replaces.as_ref()
		}

		fn queued_to_encode(&self, file: FileId) -> bool {
			self.owner.pipeline.queues[&Work::Encode].contains(file)
		}

		fn archived(&self, path: &str) -> PathBuf {
			self.owner.destination_path(&key(path))
		}

		fn messages(&self) -> (Vec<String>, Vec<(NoticeLevel, String)>) {
			let (mut logs, mut notices) = (Vec::new(), Vec::new());
			for update in self.updates.try_iter() {
				match update {
					Update::Log(entry) => logs.push(entry.message),
					Update::Notice(notice) => notices.push((notice.level, notice.text)),
					_ => {}
				}
			}
			(logs, notices)
		}

		/// The retried file's job, up to its commit intent: where its output goes, and what it may replace there.
		fn commit(&mut self, file: FileId, made: Made) -> Placement {
			for queue in self.owner.pipeline.queues.values_mut() {
				queue.remove(file);
			}
			self.owner.pipeline.jobs.insert(file, running_job(Work::Encode));
			self.owner.set_state(file, FileState::InProgress { attempt: 1 });

			let (reply, answer) = oneshot::channel();
			let staged = Staged {
				path: PathBuf::from("staged"),
				size: 9,
				hash: Hash(*blake3::hash(b"new copy!").as_bytes()),
			};
			self.owner.on_job(
				file,
				JobMessage::Commit {
					staged,
					made,
					identity: SourceIdentity::default(),
					reply,
				},
			);
			answer.blocking_recv().unwrap().unwrap()
		}

		fn placed(&mut self, file: FileId, placement: &Placement) {
			fs::write(&placement.path, b"new copy!").unwrap();
			self.owner.on_job(file, JobMessage::Placed);
		}
	}

	fn output(path: &str, how: How) -> Output {
		Output {
			how,
			path: key(path),
			size: 9,
			hash: Some(Hash([1; 32])),
		}
	}

	fn kept(path: &str) -> FileState {
		FileState::Done(output(path, How::Copied(CopyReason::KeptOriginal)))
	}

	fn placeholder(path: &str, canonical: &str) -> FileState {
		FileState::Done(output(
			path,
			How::Placeholder {
				canonical: key(canonical),
				lost: false,
			},
		))
	}

	#[test]
	fn a_kept_file_is_queued_to_replace_its_kept_copy_once_the_new_one_is_in_place() {
		let mut fixture = Fixture::new();
		let file = fixture.add("a.jpg", kept("a.jpg"));
		fixture.owner.pipeline.chosen_copies.insert(file);

		fixture.owner.retry_kept(Target::File(file));

		assert_eq!(fixture.state(file), &FileState::Queued);
		assert_eq!(fixture.replaces(file), Some(&key("a.jpg")));
		assert!(fixture.queued_to_encode(file));
		assert!(
			fixture.archived("a.jpg").exists(),
			"the kept copy stays until it is replaced"
		);
		let (logs, notices) = fixture.messages();
		assert!(logs.contains(&"a.jpg: retry chosen".to_owned()), "{logs:?}");
		assert!(notices.is_empty(), "{notices:?}");
	}

	#[test]
	fn a_folder_retry_touches_only_the_kept_files_inside_it() {
		let mut fixture = Fixture::new();
		let inside = fixture.add("Photos/a.jpg", kept("Photos/a.jpg"));
		let deeper = fixture.add("Photos/Trip/b.jpg", kept("Photos/Trip/b.jpg"));
		let encoded = fixture.add("Photos/c.jpg", FileState::Done(output("Photos/c.avif", How::Encoded)));
		let copied = fixture.add(
			"Photos/d.pdf",
			FileState::Done(output("Photos/d.pdf", How::Copied(CopyReason::NotMedia))),
		);
		let deleted = fixture.add(
			"Photos/e.jpg",
			FileState::OriginalDeleted(output("Photos/e.jpg", How::Copied(CopyReason::KeptOriginal))),
		);
		let issue = FileState::Issue {
			kind: IssueKind::Unrecoverable { damaged: false },
			attempt: 1,
			reason: "failed".into(),
			output: None,
		};
		let failed = fixture.add("Photos/f.jpg", issue.clone());
		let running = fixture.add("Photos/g.jpg", kept("Photos/g.jpg"));
		fixture.owner.pipeline.jobs.insert(running, running_job(Work::Delete));
		let outside = fixture.add("Other/h.jpg", kept("Other/h.jpg"));
		let photos = fixture.folder("Photos");

		fixture.owner.retry_kept(Target::Folder(photos));

		for file in [inside, deeper] {
			assert_eq!(fixture.state(file), &FileState::Queued);
			assert!(fixture.queued_to_encode(file));
		}
		assert_eq!(
			fixture.state(encoded),
			&FileState::Done(output("Photos/c.avif", How::Encoded))
		);
		assert!(matches!(fixture.state(copied), FileState::Done(_)));
		assert!(matches!(fixture.state(deleted), FileState::OriginalDeleted(_)));
		assert_eq!(fixture.state(failed), &issue);
		assert_eq!(fixture.state(running), &kept("Photos/g.jpg"));
		assert_eq!(fixture.state(outside), &kept("Other/h.jpg"));
		for file in [encoded, copied, deleted, failed, running, outside] {
			assert_eq!(fixture.replaces(file), None);
		}
		let (logs, _) = fixture.messages();
		assert!(
			logs.contains(&"retrying 2 kept files in Photos; 1 busy file left as it is".to_owned()),
			"{logs:?}"
		);
	}

	#[test]
	fn a_kept_canonical_copy_leaves_its_copy_to_its_placeholder() {
		let mut fixture = Fixture::new();
		let canonical = fixture.add("a.jpg", kept("a.jpg"));
		let duplicate = fixture.add("Backup/a.jpg", placeholder("Backup/a.jpg.txt", "a.jpg"));

		fixture.owner.retry_kept(Target::File(canonical));

		assert_eq!(fixture.state(duplicate), &kept("a.jpg"));
		assert_eq!(fixture.state(canonical), &FileState::Queued);
		assert_eq!(
			fixture.replaces(canonical),
			None,
			"the kept copy is the placeholder's archive now"
		);
		assert!(fixture.archived("a.jpg").exists());

		let placement = fixture.commit(canonical, Made::Copied(CopyReason::KeptOriginal));

		assert_eq!(placement.path, fixture.archived("a (2).jpg"));
		assert_eq!(placement.replaces, None);
	}

	#[test]
	fn nothing_to_retry_is_said_once_and_changes_nothing() {
		let mut fixture = Fixture::new();
		let encoded = fixture.add("Photos/a.jpg", FileState::Done(output("Photos/a.avif", How::Encoded)));
		let photos = fixture.folder("Photos");
		let empty = fixture.folder("Empty");

		for target in [Target::File(encoded), Target::Folder(photos), Target::Folder(empty)] {
			fixture.owner.retry_kept(target);
		}

		assert!(matches!(fixture.state(encoded), FileState::Done(_)));
		let (logs, notices) = fixture.messages();
		assert_eq!(
			notices,
			vec![(NoticeLevel::Info, "nothing kept to retry here".to_owned()); 3]
		);
		assert!(!logs.iter().any(|log| log.contains("retry")), "{logs:?}");
	}

	#[test]
	fn kept_again_replaces_the_kept_copy_in_its_place() {
		let mut fixture = Fixture::new();
		let file = fixture.add("a.jpg", kept("a.jpg"));
		fixture.owner.retry_kept(Target::File(file));

		let placement = fixture.commit(file, Made::Copied(CopyReason::KeptOriginal));

		assert_eq!(placement.path, fixture.archived("a.jpg"));
		assert_eq!(placement.replaces, Some(fixture.archived("a.jpg")));
		fixture.placed(file, &placement);
		assert_eq!(fs::read(fixture.archived("a.jpg")).unwrap(), b"new copy!");
		assert!(matches!(
			fixture.state(file),
			FileState::Done(output) if output.path == key("a.jpg") && output.how == How::Copied(CopyReason::KeptOriginal)
		));
		assert_eq!(fixture.replaces(file), None);
		assert!(!fixture.owner.issue_files.contains(&file));
	}

	#[test]
	fn an_encoded_retry_removes_the_kept_copy_only_once_its_output_is_in_place() {
		let mut fixture = Fixture::new();
		let file = fixture.add("a.jpg", kept("a.jpg"));
		fixture.owner.retry_kept(Target::File(file));

		let placement = fixture.commit(
			file,
			Made::Encoded {
				extension: "avif".into(),
			},
		);

		assert_eq!(placement.path, fixture.archived("a.avif"));
		assert_eq!(placement.replaces, None);
		assert!(
			fixture.archived("a.jpg").exists(),
			"kept until the new output is in place"
		);
		fixture.placed(file, &placement);
		assert!(!fixture.archived("a.jpg").exists());
		assert!(fixture.archived("a.avif").exists());
		assert!(matches!(fixture.state(file), FileState::Done(output) if output.how == How::Encoded));
	}

	#[test]
	fn a_busy_kept_file_is_left_as_it_is_and_said_to_be_busy() {
		let mut fixture = Fixture::new();
		let running = fixture.add("Photos/a.jpg", kept("Photos/a.jpg"));
		fixture.owner.pipeline.jobs.insert(running, running_job(Work::Delete));
		let holder = fixture.add("Photos/b.jpg", kept("Photos/b.jpg"));
		let taker = fixture.add("b.jpg", FileState::Ready);
		fixture.owner.plan_take_over(taker, holder);
		let photos = fixture.folder("Photos");

		for target in [Target::File(running), Target::File(holder), Target::Folder(photos)] {
			fixture.owner.retry_kept(target);
		}

		assert_eq!(fixture.state(running), &kept("Photos/a.jpg"));
		assert_eq!(fixture.state(holder), &kept("Photos/b.jpg"));
		assert_eq!(fixture.replaces(holder), None);
		let (_, notices) = fixture.messages();
		let busy = |text: &str| (NoticeLevel::Info, text.to_owned());
		assert_eq!(
			notices,
			[
				busy("it's busy; try again once it finishes"),
				busy("it's busy; try again once it finishes"),
				busy("its kept files are busy; try again once they finish"),
			]
		);
	}

	#[test]
	fn a_retry_that_fails_falls_back_to_its_kept_copy() {
		let mut fixture = Fixture::new();
		let file = fixture.add("a.jpg", kept("a.jpg"));
		fixture.owner.retry_kept(Target::File(file));
		assert_eq!(
			fixture.owner.tree.file(file).record.retrying,
			Some(output("a.jpg", How::Copied(CopyReason::KeptOriginal)))
		);

		fixture.owner.pipeline.jobs.insert(file, running_job(Work::Encode));
		fixture.owner.set_state(file, FileState::InProgress { attempt: 1 });
		fixture.owner.on_job(
			file,
			JobMessage::Finished(Err(ErrorKind::SourceDamaged("no end marker".into()).into())),
		);

		assert_eq!(fixture.state(file), &kept("a.jpg"));
		assert_eq!(fixture.replaces(file), None);
		assert_eq!(fixture.owner.tree.file(file).record.retrying, None);
		assert!(!fixture.owner.issue_files.contains(&file));
		assert!(fixture.archived("a.jpg").exists());
		let (logs, _) = fixture.messages();
		assert!(
			logs.iter().any(|log| log.starts_with("a.jpg: the retry failed: ")),
			"{logs:?}"
		);
	}

	#[test]
	fn a_retry_whose_source_vanishes_before_its_commit_keeps_its_kept_copy() {
		let mut fixture = Fixture::new();
		let queued = fixture.add("a.jpg", kept("a.jpg"));
		let running = fixture.add("b.jpg", kept("b.jpg"));
		for file in [queued, running] {
			fixture.owner.retry_kept(Target::File(file));
		}
		let job = running_job(Work::Encode);
		let cancel = job.cancel.clone();
		fixture.owner.pipeline.jobs.insert(running, job);
		fixture.owner.set_state(running, FileState::InProgress { attempt: 1 });

		for file in [queued, running] {
			fixture.owner.source_missing(file);
		}

		for (file, path) in [(queued, "a.jpg"), (running, "b.jpg")] {
			let FileState::Done(kept) = kept(path) else {
				unreachable!()
			};
			assert_eq!(fixture.state(file), &FileState::SourceGone(kept));
			assert!(!fixture.owner.tree.file(file).removed);
			assert_eq!(fixture.replaces(file), None);
			assert_eq!(fixture.owner.tree.file(file).record.retrying, None);
			assert!(fixture.archived(path).exists());
		}
		assert!(cancel.is_cancelled());
		assert!(!fixture.queued_to_encode(queued));
	}

	#[test]
	fn the_summary_names_the_folder_and_counts_the_files() {
		assert_eq!(retrying(1, 0, "Photos"), "retrying 1 kept file in Photos");
		assert_eq!(retrying(3, 0, ""), "retrying 3 kept files");
		assert_eq!(
			retrying(2, 1, "Photos"),
			"retrying 2 kept files in Photos; 1 busy file left as it is"
		);
	}
}
