//! Keeping front ends up to date (project layer spec §11.3): changes mark folders and files dirty, and a publish
//! step sends what changed at most every view interval. File rows go out only for watched folders and the filter
//! result, so the stream scales with what a front end shows.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cryarchiver_engine::engine::Stage;
use cryarchiver_engine::text::{display_name, display_path};

use super::owner::Owner;
use super::pipeline::kept_note;
use crate::issues::{IssueId, IssueKey, Problem, UNREADABLE_FOLDER_VERBS, UNREADABLE_SOURCE_VERBS, Verbs};
use crate::record::{CopyReason, FileState, How, IssueKind, MediaClass};
use crate::tree::{FileId, FolderId};
use crate::view::{
	DecisionLine, Details, Duplicates, FileRow, FileStatus, FilterEntry, FilterView, IssueView, KEPT_LABEL, LogEntry,
	LogKind, Notice, NoticeLevel, Overall, Parity, PathLink, RowState, RunState, Status, Update, Worker,
	decision_lines, size_text,
};

const THROUGHPUT_WINDOW: Duration = Duration::from_mins(5);

#[derive(Debug)]
pub(crate) struct ViewState {
	pub(super) watched: HashSet<FolderId>,
	pub(super) filter: Option<(String, HashSet<FileId>)>,
	dirty_folders: HashSet<FolderId>,
	dirty_files: HashSet<FileId>,
	pub(super) status_dirty: bool,
	pub(super) issues_dirty: bool,
	last_status: Option<Status>,
	last_overall: Option<Overall>,
	last_publish: Option<Instant>,
	throughput: VecDeque<(Instant, u64)>,
}

impl Default for ViewState {
	fn default() -> Self {
		Self {
			watched: HashSet::new(),
			filter: None,
			dirty_folders: HashSet::new(),
			dirty_files: HashSet::new(),
			status_dirty: true,
			issues_dirty: true,
			last_status: None,
			last_overall: None,
			last_publish: None,
			throughput: VecDeque::new(),
		}
	}
}

impl Owner {
	pub(super) fn send(&self, update: Update) {
		let _ = self.updates.send(update);
	}

	pub(super) fn notice(&self, level: NoticeLevel, text: String) {
		self.send(Update::Notice(Notice { level, text }));
	}

	pub(super) fn log(&self, kind: LogKind, message: String) {
		let since_midnight = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map_or(0, |since| since.as_secs() % 86_400);
		self.send(Update::Log(LogEntry {
			time: u32::try_from(since_midnight).unwrap_or(0),
			kind,
			message,
		}));
	}

	pub(super) fn touch_file(&mut self, file: FileId) {
		self.view.dirty_files.insert(file);
		self.touch_folder(self.tree.file(file).folder);
	}

	pub(super) fn touch_folder(&mut self, folder: FolderId) {
		let ancestors: Vec<FolderId> = self.tree.ancestors(folder).collect();
		for &ancestor in &ancestors {
			self.folder_may_finish(ancestor);
		}
		self.view.dirty_folders.extend(ancestors);
	}

	pub(super) fn file_added(&mut self, file: FileId) {
		let folder = self.tree.file(file).folder;
		if self.view.watched.contains(&folder) {
			self.send(Update::FileAdded {
				folder,
				row: self.row(file),
			});
		}
	}

	pub(super) fn publish_if_due(&mut self, now: Instant) {
		let due = self
			.view
			.last_publish
			.is_none_or(|last| now.saturating_duration_since(last) >= self.tuning.view_interval);
		if due {
			self.publish();
		}
	}

	pub(super) fn publish(&mut self) {
		let now = Instant::now();
		self.view.last_publish = Some(now);

		let mut folders: Vec<FolderId> = self.view.dirty_folders.drain().collect();
		folders.sort();
		for folder in folders {
			self.send(Update::Folder(folder, self.tree.stats(folder)));
		}

		let mut files: Vec<FileId> = self.view.dirty_files.drain().collect();
		files.sort();
		for file in files {
			let entry = self.tree.file(file);
			let shown = self.view.watched.contains(&entry.folder)
				|| self.view.filter.as_ref().is_some_and(|(_, hits)| hits.contains(&file));
			if shown && !entry.removed {
				self.send(Update::FileStatus {
					folder: entry.folder,
					file,
					status: self.file_status(file),
				});
			}
		}

		self.record_throughput(now);
		let overall = self.overall();
		if self.view.last_overall.as_ref() != Some(&overall) {
			self.view.last_overall = Some(overall.clone());
			self.send(Update::Overall(overall));
		}

		if self.view.issues_dirty {
			self.view.issues_dirty = false;
			let issues = self.issues_view();
			self.send(Update::Issues(issues));
		}

		if self.pipeline.workers_dirty {
			self.pipeline.workers_dirty = false;
			self.send(Update::Workers(self.workers()));
		}

		// Last, so a front end that sees the run finished already has the counts and issues it finished with.
		let status = self.status();
		if self.view.status_dirty || self.view.last_status.as_ref() != Some(&status) {
			self.view.status_dirty = false;
			self.view.last_status = Some(status.clone());
			self.send(Update::Status(status));
		}
	}

	fn workers(&self) -> Vec<Worker> {
		let over_limit = self.pipeline.governor.over_limit();
		let mut jobs: Vec<(&FileId, &super::pipeline::RunningJob)> = self.pipeline.jobs.iter().collect();
		jobs.sort_by(|a, b| b.1.fraction.total_cmp(&a.1.fraction).then(a.0.cmp(b.0)));

		jobs.iter()
			.enumerate()
			.map(|(index, (file, job))| {
				let entry = self.tree.file(**file);
				Worker {
					file: **file,
					folder: entry.folder,
					work: job.work,
					name: display_name(&entry.name),
					state: self.row_state(**file),
					stage: job.stage.map(|stage| stage_label(stage).to_owned()),
					fraction: job.fraction,
					over_limit: index >= jobs.len().saturating_sub(over_limit),
				}
			})
			.collect()
	}

	pub(super) fn status(&self) -> Status {
		let run_state = if self.stopping {
			RunState::Stopping
		} else if !self.started || self.paused {
			RunState::Paused
		} else if self.pipeline.disk.is_paused() {
			RunState::PausedLowDisk
		} else if self.parity().is_some() && self.settings.watch {
			RunState::Watching
		} else if self.parity().is_some() {
			RunState::Finished
		} else {
			RunState::Running
		};

		Status {
			run_state,
			limit_pending: self.limit_pending(),
			scan_complete: self.scan.complete,
			deleting_originals: self.settings.delete_originals,
			deleting_paused: self.settings.delete_originals && !self.delete_confirmed,
		}
	}

	pub(super) fn overall(&self) -> Overall {
		let root = self.tree.totals(FolderId::ROOT);
		let stats = self.tree.stats(FolderId::ROOT);
		let saved = root.bytes_in.saturating_sub(root.bytes_out);

		#[allow(clippy::cast_precision_loss)]
		let saved_fraction = if root.bytes_in == 0 {
			0.0
		} else {
			saved as f64 / root.bytes_in as f64
		};

		Overall {
			files_done: root.settled,
			files_active: self.pipeline.jobs.len() as u64,
			files_queued: root
				.files
				.saturating_sub(root.settled + u64::from(root.issues) + self.pipeline.jobs.len() as u64),
			files_total: root.files,
			scan_complete: self.scan.complete,
			progress: stats.progress,
			kept: root.kept,
			duplicates: root.duplicates,
			open_issues: u64::try_from(self.issue_files.len() + self.unreadable_issues().count()).unwrap_or(u64::MAX),
			saved_bytes: saved,
			saved_fraction,
			eta: self.eta(root.size.saturating_sub(root.bytes_in)),
			parity: self.parity().map(|report| Parity {
				source_files: report.source_files,
				archive_files: report.archive_files,
				excluded: report.excluded,
			}),
			originals_deleted: root.originals_deleted,
		}
	}

	fn record_throughput(&mut self, now: Instant) {
		let done = self.tree.totals(FolderId::ROOT).bytes_in;
		let samples = &mut self.view.throughput;
		if samples
			.back()
			.is_none_or(|(at, _)| now.saturating_duration_since(*at) >= Duration::from_secs(1))
		{
			samples.push_back((now, done));
		}
		while samples
			.front()
			.is_some_and(|(at, _)| now.saturating_duration_since(*at) > THROUGHPUT_WINDOW)
		{
			samples.pop_front();
		}
	}

	#[allow(
		clippy::cast_precision_loss,
		clippy::cast_possible_truncation,
		clippy::cast_sign_loss
	)]
	fn eta(&self, remaining: u64) -> Option<Duration> {
		let (first, last) = (self.view.throughput.front()?, self.view.throughput.back()?);
		let seconds = last.0.saturating_duration_since(first.0).as_secs_f64();
		let bytes = last.1.saturating_sub(first.1);
		if seconds < 5.0 || bytes == 0 {
			return None;
		}

		Some(Duration::from_secs_f64(remaining as f64 / (bytes as f64 / seconds)))
	}

	pub(super) fn row_state(&self, file: FileId) -> RowState {
		let job = self.pipeline.jobs.get(&file);
		match &self.tree.file(file).record.state {
			FileState::Queued if self.waiting(file) => RowState::Waiting,
			FileState::Ready if self.hashing(file) || self.is_duplicate(file) => RowState::Waiting,
			FileState::Discovered | FileState::Ready | FileState::Queued => RowState::Queued,
			FileState::InProgress { .. } if job.is_some_and(|job| job.video_waiting) => RowState::Waiting,
			FileState::InProgress { .. } if job.is_some_and(|job| job.stage == Some(Stage::Verify)) => {
				RowState::Verifying
			}
			FileState::InProgress { .. } => RowState::Encoding,
			FileState::Committing { .. } => RowState::Verifying,
			FileState::Done(output) => match &output.how {
				How::Encoded => RowState::Done,
				How::Copied(CopyReason::KeptOriginal) => RowState::KeptOriginal,
				How::Copied(_) => RowState::Copied,
				How::Placeholder { .. } => RowState::Duplicate,
			},
			FileState::Issue { kind, .. } => match kind {
				IssueKind::Unrecoverable { damaged: true } => RowState::SourceDamaged,
				IssueKind::Unrecoverable { damaged: false } | IssueKind::SourceUnreadable => RowState::Failed,
				IssueKind::OutputMissing => RowState::Warning,
			},
			FileState::Excluded => RowState::Excluded,
			FileState::SourceGone(_) => RowState::SourceGone,
			FileState::DeletePending(_) => RowState::DeletePending,
			FileState::OriginalDeleted(_) => RowState::OriginalDeleted,
		}
	}

	/// Queued, but held back: a retry delay, or a dependency that isn't done.
	pub(super) fn waiting(&self, file: FileId) -> bool {
		self.pipeline.waits.is_waiting(file) || self.pipeline.retry_at.iter().any(|(_, waiting)| *waiting == file)
	}

	pub(super) fn file_status(&self, file: FileId) -> FileStatus {
		let record = &self.tree.file(file).record;
		let output = record.state.output();

		FileStatus {
			state: self.row_state(file),
			progress: self.pipeline.jobs.get(&file).map(|job| job.fraction),
			codec: output.map(|output| codec_label(&output.how, &output.path.name())),
			bytes_in: record.size,
			bytes_out: output.map(|output| output.size),
		}
	}

	pub(super) fn row(&self, file: FileId) -> FileRow {
		FileRow {
			id: file,
			name: display_name(&self.tree.file(file).name),
			status: self.file_status(file),
		}
	}

	pub(super) fn watch_folder(&mut self, folder: FolderId, watched: bool) {
		if !self.tree.has_folder(folder) {
			return;
		}

		if !watched {
			self.view.watched.remove(&folder);
			return;
		}

		self.view.watched.insert(folder);
		let rows = self
			.tree
			.folder(folder)
			.files
			.iter()
			.filter(|&&file| !self.tree.file(file).removed)
			.map(|&file| self.row(file))
			.collect();
		self.send(Update::Files { folder, rows });
	}

	/// Matches and their ancestors, in display order: a folder, its subfolders, then its matching files.
	pub(super) fn set_filter(&mut self, query: String) {
		if query.is_empty() {
			self.view.filter = None;
			self.send(Update::Filter(None));
			return;
		}

		let needle = query.to_lowercase();
		let matches = |name: &OsStr| display_name(name).to_lowercase().contains(&needle);
		let mut hits: HashSet<FileId> = HashSet::new();
		let mut kept = vec![false; self.tree.folder_count()];
		for folder in self.tree.folder_ids().rev() {
			let entry = self.tree.folder(folder);
			for &file in &entry.files {
				if !self.tree.file(file).removed && matches(&self.tree.file(file).name) {
					hits.insert(file);
					kept[folder.0 as usize] = true;
				}
			}

			kept[folder.0 as usize] |= folder != FolderId::ROOT && matches(&entry.name);
			if kept[folder.0 as usize]
				&& let Some(parent) = entry.parent
			{
				kept[parent.0 as usize] = true;
			}
		}

		let mut rows = Vec::new();
		self.filter_rows(FolderId::ROOT, 0, &kept, &hits, &mut rows);
		let view = FilterView {
			query,
			matching_files: hits.len(),
			rows,
		};
		self.view.filter = Some((needle, hits));
		self.send(Update::Filter(Some(view)));
	}

	fn filter_rows(
		&self,
		folder: FolderId,
		depth: u16,
		kept: &[bool],
		hits: &HashSet<FileId>,
		rows: &mut Vec<(u16, FilterEntry)>,
	) {
		if !kept[folder.0 as usize] {
			return;
		}

		rows.push((depth, FilterEntry::Folder(folder)));

		for &child in &self.tree.folder(folder).children {
			self.filter_rows(child, depth + 1, kept, hits, rows);
		}

		for &file in &self.tree.folder(folder).files {
			if hits.contains(&file) {
				rows.push((
					depth + 1,
					FilterEntry::File {
						folder,
						row: self.row(file),
					},
				));
			}
		}
	}

	pub(super) fn send_details(&mut self, file: FileId) {
		if !self.tree.has_file(file) {
			return;
		}

		let details = self.details(file);
		self.send(Update::Details(file, Box::new(details)));
	}

	pub(super) fn details(&mut self, file: FileId) -> Details {
		let issue = self.issue_files.contains(&file).then(|| self.file_issue_view(file));
		let duplicates = self.duplicate_links(file);
		let decision = self.decision_view(file);
		let entry = self.tree.file(file);
		let record = &entry.record;
		let output = record.state.output();
		let status = self.file_status(file);

		let mut notes = self.pipeline.notes.get(&file).cloned().unwrap_or_default();
		if let Some(kept) = decision.iter().find(|line| line.label == KEPT_LABEL) {
			let shown = kept_note(&kept.value);
			notes.retain(|note| *note != shown);
		}

		let stage = self
			.pipeline
			.jobs
			.get(&file)
			.and_then(|job| job.stage)
			.map(|stage| stage_label(stage).to_owned());
		let mut plan = vec![(
			"Kind".to_owned(),
			match record.class {
				MediaClass::Media => "media",
				MediaClass::Other => "other file, copied as it is",
				MediaClass::Unknown => "not read yet",
			}
			.to_owned(),
		)];
		if let Some(codec) = &status.codec {
			plan.push(("Output".to_owned(), codec.clone()));
		}

		Details {
			source_path: display_path(&self.source_path(&entry.key)),
			destination_path: output
				.map_or_else(String::new, |output| display_path(&self.destination_path(&output.path))),
			source_info: size_text(record.size),
			state: status.state,
			stage,
			fraction: status.progress,
			bytes_in: record.size,
			bytes_out: status.bytes_out,
			plan,
			decision,
			notes,
			duplicates,
			issue,
		}
	}

	fn decision_view(&mut self, file: FileId) -> Vec<DecisionLine> {
		match self.store.latest_decision(&self.tree.file(file).key) {
			Ok(decision) => decision.as_ref().map(decision_lines).unwrap_or_default(),
			Err(err) => {
				self.fail(err);
				Vec::new()
			}
		}
	}

	fn duplicate_links(&self, file: FileId) -> Duplicates {
		let link = |other: FileId| {
			let entry = self.tree.file(other);
			PathLink {
				file: other,
				folder: entry.folder,
				path: entry.key.display(),
			}
		};

		if let Some(canonical) = self.canonical_of(file).or_else(|| self.placeholder_target(file)) {
			return Duplicates::PlaceholderOf(link(canonical));
		}

		let duplicates = self.duplicates_of(file);
		if duplicates.is_empty() {
			Duplicates::None
		} else {
			Duplicates::CanonicalOf(duplicates.into_iter().map(link).collect())
		}
	}

	fn placeholder_target(&self, file: FileId) -> Option<FileId> {
		match &self.tree.file(file).record.state.output()?.how {
			How::Placeholder { canonical, lost: false } => self.tree.file_id(canonical),
			_ => None,
		}
	}

	pub(super) fn send_issue_log(&mut self, issue: IssueId, attempt: u32) {
		let Some(IssueKey::File(file)) = self.issue_ids.key(issue).cloned() else {
			return;
		};

		let key = self.tree.file(file).key.clone();
		match self.store.read_log(&key, attempt) {
			Ok(Some(log)) => {
				let mut lines = vec![log.error];
				lines.extend(log.log_tail);
				self.send(Update::IssueLog { issue, attempt, lines });
			}
			Ok(None) => self.send(Update::IssueLog {
				issue,
				attempt,
				lines: Vec::new(),
			}),
			Err(err) => self.fail(err),
		}
	}

	pub(super) fn issues_view(&mut self) -> Vec<IssueView> {
		let mut issues: Vec<IssueView> = self
			.issue_files
			.clone()
			.into_iter()
			.map(|file| self.file_issue_view(file))
			.collect();

		for folder in self.tree.folder_ids() {
			let cycle = self.tree.folder(folder).record.cycle;
			if cycle.is_some_and(|cycle| !cycle.acknowledged) {
				let id = self.issue_ids.id(&IssueKey::Cycle(folder));
				let entry = self.tree.folder(folder);
				issues.push(IssueView {
					id,
					problem: Problem::DirectoryCycle,
					file: None,
					folder: Some(folder),
					name: display_name(&entry.name),
					location: entry.key.display(),
					reason: "this folder link leads back into its own tree, so it isn't followed".into(),
					resolution: resolution(Problem::DirectoryCycle.verbs()),
					verbs: Problem::DirectoryCycle.verbs(),
					attempts: 0,
				});
			}
		}

		let unreadable: Vec<(FolderId, String)> = self
			.unreadable_issues()
			.map(|(folder, reason)| (folder, reason.clone()))
			.collect();
		for (folder, reason) in unreadable {
			let entry = self.tree.folder(folder);
			let label = self.folder_label(folder);
			let verbs = if entry.key.is_root() {
				UNREADABLE_SOURCE_VERBS
			} else {
				UNREADABLE_FOLDER_VERBS
			};
			issues.push(IssueView {
				id: self.issue_ids.id(&IssueKey::Unreadable(folder)),
				problem: Problem::SourceUnreadable,
				file: None,
				folder: Some(folder),
				name: if entry.key.is_root() {
					label.clone()
				} else {
					display_name(&entry.name)
				},
				location: label,
				reason: format!("the folder can't be read, so nothing in it is archived or checked: {reason}"),
				resolution: resolution(verbs),
				verbs,
				attempts: 0,
			});
		}

		issues.extend(self.unknown_issues());
		issues
	}

	pub(super) fn file_issue_view(&mut self, file: FileId) -> IssueView {
		let id = self.issue_ids.id(&IssueKey::File(file));
		let entry = self.tree.file(file);
		let (problem, reason) = match &entry.record.state {
			FileState::Issue { kind, reason, .. } => (Problem::of(*kind), reason.clone()),
			_ => (Problem::EncodeFailed, String::new()),
		};

		IssueView {
			id,
			problem,
			file: Some(file),
			folder: Some(entry.folder),
			name: display_name(&entry.name),
			location: entry.key.display(),
			reason,
			resolution: resolution(problem.verbs()),
			verbs: problem.verbs(),
			attempts: self.last_attempts.get(&entry.key).copied().unwrap_or(0),
		}
	}
}

fn resolution(verbs: Verbs) -> String {
	let mut choices: Vec<String> = [verbs.retry, verbs.accept]
		.into_iter()
		.flatten()
		.map(str::to_lowercase)
		.collect();
	if verbs.exclude {
		choices.push("exclude".into());
	}
	format!("waiting for you: {}", choices.join(", "))
}

/// The engine's stage in the words the CLI uses too.
pub(super) fn stage_label(stage: Stage) -> &'static str {
	match stage {
		Stage::Detect => "detecting",
		Stage::Probe => "probing",
		Stage::Plan => "planning",
		Stage::Queued => "queued",
		Stage::WaitingForMemory => "waiting for memory",
		Stage::Analyze => "analysing",
		Stage::Sample => "sampling",
		Stage::Encode => "encoding",
		Stage::Mux => "muxing",
		Stage::Metadata => "cover art",
		Stage::Verify => "verifying",
		Stage::Commit => "saving",
	}
}

pub(super) fn codec_label(how: &How, output_name: &OsStr) -> String {
	match how {
		How::Copied(_) => "copy".into(),
		How::Placeholder { .. } => "dup".into(),
		How::Encoded => {
			let extension = std::path::Path::new(output_name)
				.extension()
				.map(|extension| extension.to_string_lossy().to_ascii_lowercase())
				.unwrap_or_default();
			match extension.as_str() {
				"avif" => "AVIF".into(),
				"jxl" => "JXL".into(),
				"opus" | "mka" => "Opus".into(),
				"mkv" | "webm" => "AV1".into(),
				other => other.to_uppercase(),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::fs;

	use cryarchiver_engine::engine::Stage;

	use super::*;
	use crate::keys::RelPath;
	use crate::project::pipeline::tests::running_job;
	use crate::project::scanning::tests::{key, list, owner};
	use crate::record::FileRecord;
	use crate::schedule::Work;

	#[test]
	fn more_jobs_over_the_limit_than_workers_shown_never_panics() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		fs::write(source.join("a.jpg"), b"a").unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		list(&mut owner, &RelPath::root(), true);
		let file = owner.tree.file_id(&key("a.jpg")).unwrap();
		let mut job = running_job(Work::Encode);
		job.stage = Some(Stage::WaitingForMemory);
		owner.pipeline.jobs.insert(file, job);
		let now = Instant::now();
		owner.pipeline.governor.started(Work::Encode, now);
		owner.pipeline.governor.started(Work::Hash, now);
		owner.pipeline.governor.started(Work::Hash, now);
		owner.pipeline.governor.set_limit(true);

		let workers = owner.workers();

		assert_eq!(workers.len(), 1);
		assert!(workers[0].over_limit);
		assert_eq!(workers[0].stage.as_deref(), Some("waiting for memory"));
	}

	#[test]
	fn a_later_start_without_the_confirmation_never_takes_it_back() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		owner.settings.delete_originals = true;

		owner.start(false);
		assert!(owner.status().deleting_paused);

		owner.start(true);
		owner.start(false);
		assert!(!owner.status().deleting_paused);
		assert!(owner.deleting());
	}

	#[test]
	fn details_show_escaped_paths_readable_sizes_and_stage_words() {
		let dir = tempfile::tempdir().unwrap();
		let source = dir.path().join("source");
		fs::create_dir_all(&source).unwrap();
		let (mut owner, _updates) = owner(&source, &dir.path().join("archive"));
		let file = owner.tree.add_file(
			FolderId::ROOT,
			OsStr::new("a\u{1b}[2Jb.jpg"),
			FileRecord::discovered(12_345_678, 1),
		);
		let mut job = running_job(Work::Encode);
		job.stage = Some(Stage::WaitingForMemory);
		owner.pipeline.jobs.insert(file, job);

		let details = owner.details(file);

		assert!(!details.source_path.contains('\u{1b}'), "{}", details.source_path);
		assert_eq!(details.source_info, "12.3 MB");
		assert_eq!(details.stage.as_deref(), Some("waiting for memory"));
	}
}
