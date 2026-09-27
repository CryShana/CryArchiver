//! The read model a front end mirrors (project layer spec §11.3). The project task sends [`Update`]s; a front end
//! folds them into its own view in its refresh step and never waits on the project. Every `String` here is
//! display-safe, escaped as `cryarchiver_engine::text::display_name` does.

use std::time::Duration;

use crate::issues::{IssueId, Problem, Verbs};
use crate::schedule::Work;
use crate::settings::Settings;
use crate::tree::{FileId, FolderId, FolderStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
	/// Not started yet, or paused by the user: nothing new starts.
	Paused,
	Running,
	PausedLowDisk,
	/// Everything is archived and watch mode waits for new files.
	Watching,
	Stopping,
	/// Everything is archived and the completion check ran.
	Finished,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
	pub run_state: RunState,
	/// Part of the performance limit waits for running jobs; restarting them applies it now.
	pub limit_pending: bool,
	pub scan_complete: bool,
	pub deleting_originals: bool,
}

/// The state a file row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowState {
	Done,
	Encoding,
	Verifying,
	Queued,
	KeptOriginal,
	Copied,
	Duplicate,
	/// A retry delay, a canonical copy, the files a reference names, or the dedup hashes.
	Waiting,
	SourceDamaged,
	Failed,
	Warning,
	SourceGone,
	DeletePending,
	OriginalDeleted,
	Excluded,
}

impl RowState {
	pub fn is_active(self) -> bool {
		matches!(self, Self::Encoding | Self::Verifying)
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileStatus {
	pub state: RowState,
	pub progress: Option<f64>,
	pub codec: Option<String>,
	pub bytes_in: u64,
	pub bytes_out: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRow {
	pub id: FileId,
	pub name: String,
	pub status: FileStatus,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Parity {
	pub source_files: u64,
	pub archive_files: u64,
	pub excluded: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overall {
	pub files_done: u64,
	pub files_active: u64,
	pub files_queued: u64,
	pub files_total: u64,
	pub scan_complete: bool,
	pub progress: f64,
	pub kept: u64,
	pub duplicates: u64,
	pub open_issues: u64,
	pub saved_bytes: u64,
	pub saved_fraction: f64,
	pub eta: Option<Duration>,
	pub parity: Option<Parity>,
	pub originals_deleted: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Worker {
	pub file: FileId,
	pub folder: FolderId,
	pub work: Work,
	pub codec: String,
	pub name: String,
	pub state: RowState,
	pub fraction: f64,
	/// The pending performance limit wouldn't let this job run; restarting cancels it.
	pub over_limit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemView {
	pub cpu_percent: u8,
	pub ram_used: u64,
	pub ram_total: u64,
	pub disk_free: u64,
	pub low_priority: bool,
	pub toolchain: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueView {
	pub id: IssueId,
	pub problem: Problem,
	pub file: Option<FileId>,
	pub folder: Option<FolderId>,
	pub name: String,
	/// Relative to the source root, or to the destination root for files the project didn't write.
	pub location: String,
	pub reason: String,
	pub resolution: String,
	pub verbs: Verbs,
	/// Attempts with a failure log, numbered from 1.
	pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterEntry {
	Folder(FolderId),
	File { folder: FolderId, row: FileRow },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilterView {
	pub query: String,
	/// Matches and their ancestors in display order, each with its depth below the root.
	pub rows: Vec<(u16, FilterEntry)>,
	pub matching_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathLink {
	pub file: FileId,
	pub folder: FolderId,
	pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Duplicates {
	None,
	PlaceholderOf(PathLink),
	CanonicalOf(Vec<PathLink>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Details {
	pub source_path: String,
	pub destination_path: String,
	pub source_info: String,
	pub state: RowState,
	pub stage: Option<String>,
	pub fraction: Option<f64>,
	pub bytes_in: u64,
	pub bytes_out: Option<u64>,
	pub plan: Vec<(String, String)>,
	pub log_tail: Vec<String>,
	pub duplicates: Duplicates,
	pub issue: Option<IssueView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
	Row(RowState),
	Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
	/// Seconds since midnight UTC.
	pub time: u32,
	pub kind: LogKind,
	pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoticeLevel {
	Info,
	Warning,
	Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
	pub level: NoticeLevel,
	pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
	Status(Status),
	Settings(Settings),
	FolderAdded {
		id: FolderId,
		parent: FolderId,
		name: String,
	},
	Folder(FolderId, FolderStats),
	/// The answer to watching a folder: its rows, in order.
	Files {
		folder: FolderId,
		rows: Vec<FileRow>,
	},
	FileAdded {
		folder: FolderId,
		row: FileRow,
	},
	FileRemoved {
		folder: FolderId,
		file: FileId,
	},
	/// A status change of a file in a watched folder or in the current filter result.
	FileStatus {
		folder: FolderId,
		file: FileId,
		status: FileStatus,
	},
	Overall(Overall),
	Workers(Vec<Worker>),
	Issues(Vec<IssueView>),
	Filter(Option<FilterView>),
	Details(FileId, Box<Details>),
	IssueLog {
		issue: IssueId,
		attempt: u32,
		lines: Vec<String>,
	},
	Log(LogEntry),
	Notice(Notice),
	System(SystemView),
	/// The project task has stopped; nothing follows.
	Stopped,
}
