//! What the TUI displays. A source keeps one [`ArchiveView`] up to date; the TUI only reads it.
//!
//! Every `String` here is display-safe: control characters and invalid UTF-8 are already escaped, as
//! `cryarchiver_engine::text::display_name` does. The TUI measures and truncates them but never escapes them again.

use std::collections::VecDeque;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveView {
	pub header: Header,
	pub settings: Settings,
	pub overall: Overall,
	pub workers: Workers,
	pub system: SystemStats,
	pub library: Library,
	pub log: Log,
	pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
	pub source: String,
	pub destination: String,
	pub codecs: String,
	/// Part of the performance limit can't reach running jobs yet; the workers it affects carry `over_limit`.
	pub limit_pending: bool,
	pub run_state: RunState,
	pub deleting_originals: bool,
	/// The data is made up, as with the mock source.
	pub demo: bool,
}

/// The performance limit as the header shows it: the stored cap plus whether it is still being applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
	pub cap: u8,
	pub pending: bool,
}

impl ArchiveView {
	pub fn limit(&self) -> Option<Limit> {
		self.settings.performance_limit.map(|cap| Limit {
			cap,
			pending: self.header.limit_pending,
		})
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
	Running,
	Paused,
	PausedLowDisk,
	/// Everything is done and watch mode waits for new files.
	Watching,
	Stopping,
	Finished,
}

/// The project settings of the project layer spec §4, as currently stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
	pub speed: Speed,
	pub min_savings: u8,
	pub watch: bool,
	/// The CPU cap in percent while the performance limit is on.
	pub performance_limit: Option<u8>,
	pub stability_window: Duration,
	pub keep_auxiliary_images: bool,
	pub delete_originals: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
	Slow,
	Moderate,
	Fast,
}

impl Speed {
	pub const fn label(self) -> &'static str {
		match self {
			Self::Slow => "SLOW",
			Self::Moderate => "MODERATE",
			Self::Fast => "FAST",
		}
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct Overall {
	/// Done or resolved by the user, as in the project layer spec §10.2.
	pub files_done: u64,
	pub files_active: u64,
	pub files_queued: u64,
	/// Files found so far; still growing while `scan_complete` is false.
	pub files_total: u64,
	pub scan_complete: bool,
	/// The root folder's fraction, in-progress files included. Open issues keep it below 1.
	pub progress: f64,
	pub kept: u64,
	pub duplicates: u64,
	pub open_issues: u64,
	pub saved_bytes: u64,
	pub saved_fraction: f64,
	/// Oldest first.
	pub savings_history: Vec<u64>,
	pub eta: Option<Duration>,
	/// Set once the run has finished and the completion check ran.
	pub parity: Option<Parity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parity {
	pub source_files: u64,
	pub archive_files: u64,
	pub excluded: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workers {
	pub jobs: Vec<WorkerJob>,
	/// For example `1 video + 3 image slots`.
	pub slots: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerJob {
	pub file: FileId,
	pub folder: NodeId,
	pub work: WorkKind,
	pub codec: String,
	pub name: String,
	pub state: FileState,
	pub activity: JobActivity,
	/// The pending performance limit would not let this job run; restarting cancels it.
	pub over_limit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
	Encode,
	Copy,
	Hash,
	Placeholder,
	Delete,
}

impl WorkKind {
	pub const fn label(self) -> &'static str {
		match self {
			Self::Encode => "encode",
			Self::Copy => "copy",
			Self::Hash => "hash",
			Self::Placeholder => "placeholder",
			Self::Delete => "delete",
		}
	}
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobActivity {
	Progress {
		fraction: f64,
		fps: Option<f64>,
		speed: Option<f64>,
	},
	QualitySearch {
		probe: u32,
		probes: u32,
		crf: f64,
		metric: String,
		score: f64,
	},
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemStats {
	pub cpu_percent: u8,
	/// Oldest first, in percent.
	pub cpu_history: Vec<u64>,
	pub ram_used: u64,
	pub ram_total: u64,
	pub disk_label: String,
	pub disk_free: u64,
	pub low_priority: bool,
	pub toolchain: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
	pub const fn index(self) -> usize {
		self.0 as usize
	}
}

/// Identifies one source file for the whole session. The source hands them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u64);

/// A folder of the archive tree. Files are not nodes; a folder's files come from `ArchiveSource::files`.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
	pub name: String,
	pub parent: Option<NodeId>,
	pub children: Vec<NodeId>,
	pub stats: FolderStats,
}

/// Rolled up from the folder's direct children, so every file below it weighs the same (project layer spec §10.2).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FolderStats {
	pub files_done: u64,
	pub files_total: u64,
	pub progress: f64,
	/// Of the files finished so far.
	pub bytes_in: u64,
	/// Of the files finished so far.
	pub bytes_out: u64,
	pub open_issues: u32,
	/// Every descendant is done or resolved and the whole subtree has been scanned.
	pub complete: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileRow {
	pub id: FileId,
	pub name: String,
	pub status: FileStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileStatus {
	pub state: FileState,
	pub progress: Option<f64>,
	pub codec: Option<String>,
	pub bytes_in: u64,
	pub bytes_out: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileState {
	Done,
	Encoding,
	QualitySearch,
	Verifying,
	Queued,
	KeptOriginal,
	Copied,
	Duplicate,
	Waiting,
	SourceDamaged,
	Failed,
	Warning,
	SourceGone,
	DeletePending,
	OriginalDeleted,
	Excluded,
}

impl FileState {
	pub const fn is_active(self) -> bool {
		matches!(self, Self::Encoding | Self::QualitySearch | Self::Verifying)
	}
}

/// The folder tree as an arena. Folders are only ever added, so a [`NodeId`] stays valid for the whole session.
#[derive(Debug, Clone, PartialEq)]
pub struct Library {
	nodes: Vec<Node>,
	structure_version: u64,
}

impl Library {
	pub fn new(root_name: impl Into<String>) -> Self {
		Self {
			nodes: vec![Node {
				name: root_name.into(),
				parent: None,
				children: Vec::new(),
				stats: FolderStats::default(),
			}],
			structure_version: 0,
		}
	}

	pub const fn root(&self) -> NodeId {
		NodeId(0)
	}

	pub fn len(&self) -> usize {
		self.nodes.len()
	}

	pub fn is_empty(&self) -> bool {
		self.nodes.is_empty()
	}

	/// Changes whenever a folder is added. Values inside folders can change without bumping it.
	pub const fn structure_version(&self) -> u64 {
		self.structure_version
	}

	pub fn node(&self, id: NodeId) -> &Node {
		&self.nodes[id.index()]
	}

	pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
		&mut self.nodes[id.index()]
	}

	/// Ascending ids. A folder is always added after its parent, so its id is greater and a reverse walk visits
	/// children before their parents.
	pub fn ids(&self) -> impl DoubleEndedIterator<Item = NodeId> + use<> {
		(0..u32::try_from(self.nodes.len()).expect("fewer than 2^32 folders")).map(NodeId)
	}

	pub fn add_folder(&mut self, parent: NodeId, name: impl Into<String>) -> NodeId {
		let id = NodeId(u32::try_from(self.nodes.len()).expect("fewer than 2^32 folders"));
		self.nodes.push(Node {
			name: name.into(),
			parent: Some(parent),
			children: Vec::new(),
			stats: FolderStats::default(),
		});
		self.nodes[parent.index()].children.push(id);
		self.structure_version += 1;
		id
	}

	pub fn stats(&self, id: NodeId) -> &FolderStats {
		&self.node(id).stats
	}

	/// Path from the root's first child down to `id`, joined with `/`.
	pub fn relative_path(&self, id: NodeId) -> String {
		let mut names = Vec::new();
		let mut current = Some(id);
		while let Some(folder) = current {
			let node = self.node(folder);
			if node.parent.is_some() {
				names.push(node.name.as_str());
			}
			current = node.parent;
		}

		names.reverse();
		names.join("/")
	}

	pub fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
		std::iter::successors(self.node(id).parent, |&folder| self.node(folder).parent)
	}
}

/// The tree narrowed to a name filter: matches plus their ancestors, in display order.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterResult {
	pub query: String,
	pub rows: Vec<FilterRow>,
	pub matching_files: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilterRow {
	pub depth: u16,
	pub entry: FilterEntry,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterEntry {
	Folder(NodeId),
	File { folder: NodeId, row: FileRow },
}

/// Seconds since midnight, shown as `hh:mm:ss`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockTime(pub u32);

impl std::fmt::Display for ClockTime {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let seconds = self.0 % 86_400;
		write!(f, "{:02}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
	pub time: ClockTime,
	pub marker: LogMarker,
	pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMarker {
	State(FileState),
	Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
	lines: VecDeque<LogLine>,
	capacity: usize,
}

impl Log {
	pub fn with_capacity(capacity: usize) -> Self {
		Self {
			lines: VecDeque::with_capacity(capacity),
			capacity: capacity.max(1),
		}
	}

	pub fn push(&mut self, line: LogLine) {
		if self.lines.len() == self.capacity {
			self.lines.pop_back();
		}
		self.lines.push_front(line);
	}

	pub fn newest_first(&self) -> impl Iterator<Item = &LogLine> {
		self.lines.iter()
	}

	pub fn latest(&self) -> Option<&LogLine> {
		self.lines.front()
	}

	pub fn len(&self) -> usize {
		self.lines.len()
	}

	pub fn is_empty(&self) -> bool {
		self.lines.is_empty()
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
	pub level: NoticeLevel,
	pub text: String,
}

/// Ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoticeLevel {
	Info,
	Warning,
	Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IssueId(pub u64);

/// The issue kinds of the project layer spec §10.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind {
	SourceDamaged,
	EncodeFailed,
	SourceUnreadable,
	OutputMissing,
	UnknownFile,
	DirectoryCycle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
	pub id: IssueId,
	pub kind: IssueKind,
	/// `None` for issues without a source file: unknown destination files and directory cycles.
	pub file: Option<FileId>,
	pub folder: Option<NodeId>,
	pub name: String,
	/// Where the issue is, relative to the source or the destination root.
	pub location: String,
	pub reason: String,
	pub resolution: String,
	pub verbs: IssueVerbs,
	/// Attempts with a failure log, numbered from 1; fetch one with `ArchiveSource::issue_log`.
	pub attempts: u32,
}

/// The resolutions an issue kind allows. A `Some` label is the kind's own name for the verb, such as "Copy original".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueVerbs {
	pub retry: Option<String>,
	pub accept: Option<String>,
	pub exclude: bool,
}

impl IssueVerbs {
	pub fn label(&self, verb: Verb) -> Option<&str> {
		match verb {
			Verb::Retry => self.retry.as_deref(),
			Verb::Accept => self.accept.as_deref(),
			Verb::Exclude => self.exclude.then_some("Exclude"),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
	Retry,
	Accept,
	Exclude,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDetails {
	pub source_path: String,
	pub destination_path: String,
	pub source_info: String,
	pub state: FileState,
	pub stages: Vec<StageMark>,
	pub live: Option<LiveProgress>,
	pub bytes_in: u64,
	pub bytes_out: Option<u64>,
	pub estimated_out: Option<u64>,
	pub plan: Vec<PlanLine>,
	pub quality: Option<String>,
	pub log_tail: Vec<String>,
	pub duplicates: Duplicates,
	pub issue: Option<IssueSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageMark {
	pub name: String,
	pub progress: StageProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageProgress {
	Done,
	Current,
	Pending,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveProgress {
	pub fraction: f64,
	pub fps: Option<f64>,
	pub speed: Option<f64>,
	pub eta: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLine {
	pub label: String,
	pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Duplicates {
	None,
	PlaceholderOf(PathLink),
	CanonicalOf(Vec<PathLink>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathLink {
	pub file: FileId,
	pub folder: NodeId,
	pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSummary {
	pub id: IssueId,
	pub kind: IssueKind,
	pub reason: String,
	pub resolution: String,
	pub verbs: IssueVerbs,
}
