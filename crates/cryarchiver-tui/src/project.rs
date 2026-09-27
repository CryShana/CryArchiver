//! The real archive source: a UI-side mirror of a [`Project`], folded from the project task's updates in
//! [`ArchiveSource::refresh`], so the UI thread never waits on the project.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use cryarchiver_engine::text::display_path;
use cryarchiver_project::issues::{IssueId as ProjectIssueId, Problem, Verb as ProjectVerb};
use cryarchiver_project::settings::{
	Settings as ProjectSettings, SettingsChange as ProjectChange, Speed as ProjectSpeed,
};
use cryarchiver_project::tree::{FileId as ProjectFileId, FolderId, FolderStats as ProjectStats};
use cryarchiver_project::view::{self as project_view, RowState, Update};
use cryarchiver_project::{Command, Project, Target as ProjectTarget};

use crate::source::{Action, ArchiveSource, SettingsChange, Target};
use crate::view::{
	ArchiveView, ClockTime, Duplicates, FileDetails, FileId, FileRow, FileState, FileStatus, FilterEntry, FilterResult,
	FilterRow, FolderStats, Header, Issue, IssueId, IssueKind, IssueSummary, IssueVerbs, JobActivity, Library,
	LiveProgress, Log, LogLine, LogMarker, NodeId, NoticeLevel, Notification, Overall, Parity, PathLink, PlanLine,
	RunState, Settings, Speed, StageMark, SystemStats, Verb, WorkKind, WorkerJob, Workers,
};

const LOG_CAPACITY: usize = 500;
const HISTORY_LEN: usize = 60;
const UPDATES_PER_REFRESH: usize = 20_000;
const DETAILS_REFRESH: Duration = Duration::from_millis(500);

type AttemptLog = Vec<String>;

pub struct ProjectSource {
	project: Project,
	view: ArchiveView,
	nodes: Vec<NodeId>,
	folders: HashMap<NodeId, FolderId>,
	rows: HashMap<NodeId, Vec<FileRow>>,
	positions: HashMap<FileId, (NodeId, usize)>,
	rows_version: u64,
	filter: Option<FilterResult>,
	filter_positions: HashMap<FileId, usize>,
	details: RefCell<HashMap<FileId, (Instant, project_view::Details)>>,
	requested_details: RefCell<HashMap<FileId, Instant>>,
	issue_logs: RefCell<HashMap<(IssueId, u32), Option<AttemptLog>>>,
	notifications: Vec<Notification>,
	savings: VecDeque<u64>,
	cpu: VecDeque<u64>,
	last_history: Option<Instant>,
}

impl ProjectSource {
	/// Wraps an open project and starts it: the TUI's confirmation for deleting originals is the typed `DELETE`
	/// that turned the setting on.
	pub fn new(project: Project) -> Self {
		let settings = project.settings().clone();
		let library = Library::new(display_path(&settings.source));
		let root = library.root();

		project.send(Command::Start { delete_confirmed: true });

		Self {
			view: ArchiveView {
				header: Header {
					source: display_path(&settings.source),
					destination: display_path(&settings.destination),
					codecs: "LATEST".into(),
					limit_pending: false,
					run_state: RunState::Paused,
					deleting_originals: settings.delete_originals,
					demo: false,
				},
				settings: settings_view(&settings),
				overall: Overall {
					files_done: 0,
					files_active: 0,
					files_queued: 0,
					files_total: 0,
					scan_complete: false,
					progress: 0.0,
					kept: 0,
					duplicates: 0,
					open_issues: 0,
					saved_bytes: 0,
					saved_fraction: 0.0,
					savings_history: Vec::new(),
					eta: None,
					parity: None,
				},
				workers: Workers {
					jobs: Vec::new(),
					slots: String::new(),
				},
				system: SystemStats {
					cpu_percent: 0,
					cpu_history: Vec::new(),
					ram_used: 0,
					ram_total: 0,
					disk_label: display_path(&settings.destination),
					disk_free: 0,
					low_priority: settings.performance_limit.is_some(),
					toolchain: String::new(),
				},
				library,
				log: Log::with_capacity(LOG_CAPACITY),
				issues: Vec::new(),
			},
			nodes: vec![root],
			folders: HashMap::from([(root, FolderId::ROOT)]),
			rows: HashMap::new(),
			positions: HashMap::new(),
			rows_version: 0,
			filter: None,
			filter_positions: HashMap::new(),
			details: RefCell::new(HashMap::new()),
			requested_details: RefCell::new(HashMap::new()),
			issue_logs: RefCell::new(HashMap::new()),
			notifications: Vec::new(),
			savings: VecDeque::new(),
			cpu: VecDeque::new(),
			last_history: None,
			project,
		}
	}

	fn shutdown_on_own_thread(&mut self) {
		let project = &mut self.project;
		std::thread::scope(|scope| {
			scope.spawn(|| project.shutdown());
		});
	}

	fn node(&self, folder: FolderId) -> NodeId {
		self.nodes
			.get(folder.0 as usize)
			.copied()
			.unwrap_or_else(|| self.view.library.root())
	}

	fn folder(&self, node: NodeId) -> Option<FolderId> {
		self.folders.get(&node).copied()
	}

	fn apply(&mut self, update: Update) {
		match update {
			Update::Status(status) => {
				self.view.header.run_state = run_state(status.run_state);
				self.view.header.limit_pending = status.limit_pending;
				self.view.header.deleting_originals = status.deleting_originals;
				self.view.overall.scan_complete = status.scan_complete;
			}
			Update::Settings(settings) => {
				self.view.settings = settings_view(&settings);
				self.view.header.deleting_originals = settings.delete_originals;
			}
			Update::FolderAdded { id, parent, name } => {
				let node = self.view.library.add_folder(self.node(parent), name);
				if self.nodes.len() <= id.0 as usize {
					self.nodes.resize(id.0 as usize + 1, self.view.library.root());
				}
				self.nodes[id.0 as usize] = node;
				self.folders.insert(node, id);
			}
			Update::Folder(folder, stats) => {
				let node = self.node(folder);
				self.view.library.node_mut(node).stats = folder_stats(stats);
			}
			Update::Files { folder, rows } => self.load_rows(folder, rows),
			Update::FileAdded { folder, row } => self.add_row(folder, row),
			Update::FileRemoved { folder, file } => self.remove_row(folder, file),
			Update::FileStatus { file, status, .. } => self.patch_status(file, &status),
			Update::Overall(overall) => self.apply_overall(&overall),
			Update::Workers(workers) => self.apply_workers(workers),
			Update::Issues(issues) => {
				self.view.issues = issues.iter().map(|issue| self.issue(issue)).collect();
			}
			Update::Filter(filter) => self.apply_filter(filter),
			Update::Details(file, details) => {
				self.details
					.borrow_mut()
					.insert(FileId(file.0), (Instant::now(), *details));
			}
			Update::IssueLog { issue, attempt, lines } => {
				self.issue_logs
					.borrow_mut()
					.insert((IssueId(issue.0), attempt), Some(lines));
			}
			Update::Log(entry) => self.view.log.push(LogLine {
				time: ClockTime(entry.time),
				marker: match entry.kind {
					project_view::LogKind::Row(state) => LogMarker::State(file_state(state)),
					project_view::LogKind::Action => LogMarker::Action,
				},
				message: entry.message,
			}),
			Update::Notice(notice) => self.notifications.push(Notification {
				level: match notice.level {
					project_view::NoticeLevel::Info => NoticeLevel::Info,
					project_view::NoticeLevel::Warning => NoticeLevel::Warning,
					project_view::NoticeLevel::Error => NoticeLevel::Error,
				},
				text: notice.text,
			}),
			Update::System(system) => {
				self.view.system.cpu_percent = system.cpu_percent;
				self.view.system.ram_used = system.ram_used;
				self.view.system.ram_total = system.ram_total;
				self.view.system.disk_free = system.disk_free;
				self.view.system.low_priority = system.low_priority;
				self.view.system.toolchain = system.toolchain;
				push_history(&mut self.cpu, u64::from(system.cpu_percent));
				self.view.system.cpu_history = self.cpu.iter().copied().collect();
			}
			Update::Stopped => self.view.header.run_state = RunState::Stopping,
		}
	}

	fn load_rows(&mut self, folder: FolderId, rows: Vec<project_view::FileRow>) {
		let node = self.node(folder);
		let rows: Vec<FileRow> = rows.into_iter().map(file_row).collect();
		for (index, row) in rows.iter().enumerate() {
			self.positions.insert(row.id, (node, index));
		}
		self.rows.insert(node, rows);
		self.rows_version += 1;
	}

	fn add_row(&mut self, folder: FolderId, row: project_view::FileRow) {
		let node = self.node(folder);
		let Some(rows) = self.rows.get_mut(&node) else {
			return;
		};

		let row = file_row(row);
		self.positions.insert(row.id, (node, rows.len()));
		rows.push(row);
		self.rows_version += 1;
	}

	fn remove_row(&mut self, folder: FolderId, file: ProjectFileId) {
		let node = self.node(folder);
		let Some(rows) = self.rows.get_mut(&node) else {
			return;
		};

		rows.retain(|row| row.id != FileId(file.0));
		self.positions.remove(&FileId(file.0));
		for (index, row) in rows.iter().enumerate() {
			self.positions.insert(row.id, (node, index));
		}
		self.rows_version += 1;
	}

	/// Statuses change in place, in the folder rows and the filter rows alike; the rows version stays.
	fn patch_status(&mut self, file: ProjectFileId, status: &project_view::FileStatus) {
		let id = FileId(file.0);
		let status = file_status(status);

		if let Some(&(node, index)) = self.positions.get(&id)
			&& let Some(row) = self.rows.get_mut(&node).and_then(|rows| rows.get_mut(index))
		{
			row.status = status.clone();
		}

		if let (Some(&index), Some(filter)) = (self.filter_positions.get(&id), self.filter.as_mut())
			&& let Some(FilterEntry::File { row, .. }) = filter.rows.get_mut(index).map(|row| &mut row.entry)
		{
			row.status = status;
		}
	}

	fn apply_overall(&mut self, overall: &project_view::Overall) {
		let now = Instant::now();
		if self
			.last_history
			.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
		{
			self.last_history = Some(now);
			push_history(&mut self.savings, overall.saved_bytes);
		}

		self.view.overall = Overall {
			files_done: overall.files_done,
			files_active: overall.files_active,
			files_queued: overall.files_queued,
			files_total: overall.files_total,
			scan_complete: overall.scan_complete,
			progress: overall.progress,
			kept: overall.kept,
			duplicates: overall.duplicates,
			open_issues: overall.open_issues,
			saved_bytes: overall.saved_bytes,
			saved_fraction: overall.saved_fraction,
			savings_history: self.savings.iter().copied().collect(),
			eta: overall.eta,
			parity: overall.parity.map(|parity| Parity {
				source_files: parity.source_files,
				archive_files: parity.archive_files,
				excluded: parity.excluded,
			}),
		};
	}

	fn apply_workers(&mut self, workers: Vec<project_view::Worker>) {
		let running = workers.len();
		self.view.workers.jobs = workers
			.into_iter()
			.map(|worker| WorkerJob {
				file: FileId(worker.file.0),
				folder: self.node(worker.folder),
				work: work_kind(worker.work),
				codec: worker.codec,
				name: worker.name,
				state: file_state(worker.state),
				activity: JobActivity::Progress {
					fraction: worker.fraction,
					fps: None,
					speed: None,
				},
				over_limit: worker.over_limit,
			})
			.collect();
		self.view.workers.slots = if self.view.settings.performance_limit.is_some() {
			"limit: one job at a time".into()
		} else {
			format!("{running} running")
		};
	}

	fn apply_filter(&mut self, filter: Option<project_view::FilterView>) {
		self.filter_positions.clear();
		self.filter = filter.map(|filter| {
			let rows: Vec<FilterRow> = filter
				.rows
				.into_iter()
				.map(|(depth, entry)| FilterRow {
					depth,
					entry: match entry {
						project_view::FilterEntry::Folder(folder) => FilterEntry::Folder(self.node(folder)),
						project_view::FilterEntry::File { folder, row } => FilterEntry::File {
							folder: self.node(folder),
							row: file_row(row),
						},
					},
				})
				.collect();

			for (index, row) in rows.iter().enumerate() {
				if let FilterEntry::File { row, .. } = &row.entry {
					self.filter_positions.insert(row.id, index);
				}
			}

			FilterResult {
				query: filter.query,
				rows,
				matching_files: filter.matching_files,
			}
		});
		self.rows_version += 1;
	}

	fn issue(&self, issue: &project_view::IssueView) -> Issue {
		Issue {
			id: IssueId(issue.id.0),
			kind: issue_kind(issue.problem),
			file: issue.file.map(|file| FileId(file.0)),
			folder: issue.folder.map(|folder| self.node(folder)),
			name: issue.name.clone(),
			location: issue.location.clone(),
			reason: issue.reason.clone(),
			resolution: issue.resolution.clone(),
			verbs: IssueVerbs {
				retry: issue.verbs.retry.map(str::to_owned),
				accept: issue.verbs.accept.map(str::to_owned),
				exclude: issue.verbs.exclude,
			},
			attempts: issue.attempts,
		}
	}

	fn target(&self, target: Target) -> Option<ProjectTarget> {
		Some(match target {
			Target::Folder(node) => ProjectTarget::Folder(self.folder(node)?),
			Target::File(file) => ProjectTarget::File(ProjectFileId(file.0)),
		})
	}

	fn change(change: SettingsChange) -> ProjectChange {
		match change {
			SettingsChange::Speed(speed) => ProjectChange::Speed(match speed {
				Speed::Slow => ProjectSpeed::Slow,
				Speed::Moderate => ProjectSpeed::Moderate,
				Speed::Fast => ProjectSpeed::Fast,
			}),
			SettingsChange::MinSavings(percent) => ProjectChange::MinSavings(percent),
			SettingsChange::Watch(watch) => ProjectChange::Watch(watch),
			SettingsChange::PerformanceLimit(cap) => ProjectChange::PerformanceLimit(cap),
			SettingsChange::StabilityWindow(window) => ProjectChange::StabilityWindow(window),
			SettingsChange::KeepAuxiliaryImages(keep) => ProjectChange::KeepAuxiliaryImages(keep),
			SettingsChange::DeleteOriginals { enabled, confirmation } => {
				ProjectChange::DeleteOriginals { enabled, confirmation }
			}
		}
	}

	fn build_details(&self, details: &project_view::Details) -> FileDetails {
		FileDetails {
			source_path: details.source_path.clone(),
			destination_path: details.destination_path.clone(),
			source_info: details.source_info.clone(),
			state: file_state(details.state),
			stages: details
				.stage
				.iter()
				.map(|stage| StageMark {
					name: stage.to_lowercase(),
					progress: crate::view::StageProgress::Current,
				})
				.collect(),
			live: details.fraction.map(|fraction| LiveProgress {
				fraction,
				fps: None,
				speed: None,
				eta: None,
			}),
			bytes_in: details.bytes_in,
			bytes_out: details.bytes_out,
			estimated_out: None,
			plan: details
				.plan
				.iter()
				.map(|(label, value)| PlanLine {
					label: label.clone(),
					value: value.clone(),
				})
				.collect(),
			quality: None,
			log_tail: details.log_tail.clone(),
			duplicates: match &details.duplicates {
				project_view::Duplicates::None => Duplicates::None,
				project_view::Duplicates::PlaceholderOf(link) => Duplicates::PlaceholderOf(self.link(link)),
				project_view::Duplicates::CanonicalOf(links) => {
					Duplicates::CanonicalOf(links.iter().map(|link| self.link(link)).collect())
				}
			},
			issue: details.issue.as_ref().map(|issue| {
				let issue = self.issue(issue);
				IssueSummary {
					id: issue.id,
					kind: issue.kind,
					reason: issue.reason,
					resolution: issue.resolution,
					verbs: issue.verbs,
				}
			}),
		}
	}

	fn link(&self, link: &project_view::PathLink) -> PathLink {
		PathLink {
			file: FileId(link.file.0),
			folder: self.node(link.folder),
			path: link.path.clone(),
		}
	}
}

impl ArchiveSource for ProjectSource {
	fn refresh(&mut self) -> bool {
		let mut changed = false;
		for _ in 0..UPDATES_PER_REFRESH {
			let Some(update) = self.project.try_update() else {
				break;
			};
			self.apply(update);
			changed = true;
		}

		changed
	}

	fn view(&self) -> &ArchiveView {
		&self.view
	}

	fn files(&self, folder: NodeId) -> Option<&[FileRow]> {
		self.rows.get(&folder).map(Vec::as_slice)
	}

	fn rows_version(&self) -> u64 {
		self.rows_version
	}

	fn filter(&self) -> Option<&FilterResult> {
		self.filter.as_ref()
	}

	fn details(&self, file: FileId) -> Option<FileDetails> {
		let now = Instant::now();
		let stale = self
			.details
			.borrow()
			.get(&file)
			.is_none_or(|(at, _)| now.duration_since(*at) >= DETAILS_REFRESH);
		let asked = self
			.requested_details
			.borrow()
			.get(&file)
			.is_some_and(|at| now.duration_since(*at) < DETAILS_REFRESH);

		if stale && !asked {
			self.requested_details.borrow_mut().insert(file, now);
			self.project.send(Command::Details(ProjectFileId(file.0)));
		}

		self.details
			.borrow()
			.get(&file)
			.map(|(_, details)| self.build_details(details))
	}

	fn issue_log(&self, issue: IssueId, attempt: u32) -> Option<Vec<String>> {
		let known = self.issue_logs.borrow().get(&(issue, attempt)).cloned();
		if let Some(lines) = known {
			return lines;
		}

		self.issue_logs.borrow_mut().insert((issue, attempt), None);
		self.project.send(Command::IssueLog(ProjectIssueId(issue.0), attempt));
		None
	}

	fn take_notifications(&mut self) -> Vec<Notification> {
		std::mem::take(&mut self.notifications)
	}

	fn perform(&mut self, action: Action) {
		let command = match action {
			Action::Start => Command::Start { delete_confirmed: true },
			Action::Pause => Command::Pause,
			Action::StopNow => Command::StopNow,
			Action::Rescan => Command::Rescan,
			Action::RestartPendingJobs => Command::RestartPendingJobs,
			Action::ChangeSettings(change) => Command::ChangeSettings(Self::change(change)),
			Action::Resolve(issue, verb) => Command::Resolve(
				ProjectIssueId(issue.0),
				match verb {
					Verb::Retry => ProjectVerb::Retry,
					Verb::Accept => ProjectVerb::Accept,
					Verb::Exclude => ProjectVerb::Exclude,
				},
			),
			Action::RetryFailedIn(node) => {
				let Some(folder) = self.folder(node) else { return };
				Command::RetryFailedIn(folder)
			}
			Action::Expanded(node, open) => {
				let Some(folder) = self.folder(node) else { return };
				if !open && let Some(rows) = self.rows.remove(&node) {
					for row in rows {
						self.positions.remove(&row.id);
					}
					self.rows_version += 1;
				}
				Command::Watch(folder, open)
			}
			Action::SetFilter(query) => {
				if query.is_empty() && self.filter.take().is_some() {
					self.filter_positions.clear();
					self.rows_version += 1;
				}
				Command::Filter(query)
			}
			Action::OpenSourceFolder(target) => {
				let Some(target) = self.target(target) else { return };
				Command::Reveal {
					target,
					destination: false,
				}
			}
			Action::OpenOutputFolder(target) => {
				let Some(target) = self.target(target) else { return };
				Command::Reveal {
					target,
					destination: true,
				}
			}
			Action::CopyPath(target) => {
				let Some(target) = self.target(target) else { return };
				Command::ShowPath(target)
			}
		};
		self.project.send(command);
	}

	fn shutdown(&mut self) {
		self.view.header.run_state = RunState::Stopping;
		self.shutdown_on_own_thread();
	}
}

impl Drop for ProjectSource {
	fn drop(&mut self) {
		self.shutdown_on_own_thread();
	}
}

fn settings_view(settings: &ProjectSettings) -> Settings {
	Settings {
		speed: match settings.speed {
			ProjectSpeed::Slow => Speed::Slow,
			ProjectSpeed::Moderate => Speed::Moderate,
			ProjectSpeed::Fast => Speed::Fast,
		},
		min_savings: settings.min_savings,
		watch: settings.watch,
		performance_limit: settings.performance_limit,
		stability_window: settings.stability_window,
		keep_auxiliary_images: settings.keep_auxiliary_images,
		delete_originals: settings.delete_originals,
	}
}

fn run_state(state: project_view::RunState) -> RunState {
	match state {
		project_view::RunState::Paused => RunState::Paused,
		project_view::RunState::Running => RunState::Running,
		project_view::RunState::PausedLowDisk => RunState::PausedLowDisk,
		project_view::RunState::Watching => RunState::Watching,
		project_view::RunState::Stopping => RunState::Stopping,
		project_view::RunState::Finished => RunState::Finished,
	}
}

fn file_state(state: RowState) -> FileState {
	match state {
		RowState::Done => FileState::Done,
		RowState::Encoding => FileState::Encoding,
		RowState::Verifying => FileState::Verifying,
		RowState::Queued => FileState::Queued,
		RowState::KeptOriginal => FileState::KeptOriginal,
		RowState::Copied => FileState::Copied,
		RowState::Duplicate => FileState::Duplicate,
		RowState::Waiting => FileState::Waiting,
		RowState::SourceDamaged => FileState::SourceDamaged,
		RowState::Failed => FileState::Failed,
		RowState::Warning => FileState::Warning,
		RowState::SourceGone => FileState::SourceGone,
		RowState::DeletePending => FileState::DeletePending,
		RowState::OriginalDeleted => FileState::OriginalDeleted,
		RowState::Excluded => FileState::Excluded,
	}
}

fn file_status(status: &project_view::FileStatus) -> FileStatus {
	FileStatus {
		state: file_state(status.state),
		progress: status.progress,
		codec: status.codec.clone(),
		bytes_in: status.bytes_in,
		bytes_out: status.bytes_out,
	}
}

fn file_row(row: project_view::FileRow) -> FileRow {
	FileRow {
		id: FileId(row.id.0),
		name: row.name,
		status: file_status(&row.status),
	}
}

fn folder_stats(stats: ProjectStats) -> FolderStats {
	FolderStats {
		files_done: stats.files_done,
		files_total: stats.files_total,
		progress: stats.progress,
		bytes_in: stats.bytes_in,
		bytes_out: stats.bytes_out,
		open_issues: stats.open_issues,
		complete: stats.complete,
	}
}

fn work_kind(work: cryarchiver_project::schedule::Work) -> WorkKind {
	use cryarchiver_project::schedule::Work;

	match work {
		Work::Encode => WorkKind::Encode,
		Work::Copy => WorkKind::Copy,
		Work::Hash => WorkKind::Hash,
		Work::Placeholder => WorkKind::Placeholder,
		Work::Delete => WorkKind::Delete,
	}
}

fn issue_kind(problem: Problem) -> IssueKind {
	match problem {
		Problem::SourceDamaged => IssueKind::SourceDamaged,
		Problem::EncodeFailed => IssueKind::EncodeFailed,
		Problem::SourceUnreadable => IssueKind::SourceUnreadable,
		Problem::OutputMissing => IssueKind::OutputMissing,
		Problem::UnknownFile => IssueKind::UnknownFile,
		Problem::DirectoryCycle => IssueKind::DirectoryCycle,
	}
}

fn push_history(history: &mut VecDeque<u64>, value: u64) {
	if history.len() == HISTORY_LEN {
		history.pop_front();
	}
	history.push_back(value);
}
