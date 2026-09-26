//! What the TUI displays. A source keeps one [`ArchiveView`] up to date; the TUI only reads it.
//!
//! Every `String` here is display-safe: control characters and invalid UTF-8 are already escaped, as
//! `cryarchiver_engine::text::display_name` does. The TUI measures and truncates them but never escapes them again.

use std::collections::VecDeque;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveView {
	pub header: Header,
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
	pub speed: String,
	pub codecs: String,
	pub performance_limit: Option<u8>,
	pub run_state: RunState,
	/// The data is made up, as with the mock source.
	pub demo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
	Running,
	Paused,
	Stopping,
	Finished,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Overall {
	/// Done or resolved by the user, as in the engine spec §22.1.
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
	pub file: NodeId,
	pub codec: String,
	pub name: String,
	pub state: FileState,
	pub activity: JobActivity,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
	pub name: String,
	pub parent: Option<NodeId>,
	pub children: Vec<NodeId>,
	pub entry: Entry,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
	Folder(FolderStats),
	File(FileStatus),
}

/// Rolled up from the folder's direct children, so every file below it weighs the same (engine spec §22.1).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FolderStats {
	pub files_done: u64,
	pub files_total: u64,
	pub progress: f64,
	pub bytes_in: u64,
	pub bytes_out: u64,
	pub open_issues: u32,
	/// Every descendant is done or resolved and the whole subtree has been scanned.
	pub complete: bool,
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
}

impl FileState {
	pub const fn is_active(self) -> bool {
		matches!(self, Self::Encoding | Self::QualitySearch | Self::Verifying)
	}
}

/// The archive tree as an arena. Nodes are only ever added, so a [`NodeId`] stays valid for the whole session.
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
				entry: Entry::Folder(FolderStats::default()),
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

	/// Changes whenever a node is added. Values inside nodes can change without bumping it.
	pub const fn structure_version(&self) -> u64 {
		self.structure_version
	}

	pub fn node(&self, id: NodeId) -> &Node {
		&self.nodes[id.index()]
	}

	pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
		&mut self.nodes[id.index()]
	}

	/// Ascending ids. A node is always added after its parent, so its id is greater and a reverse walk visits
	/// children before their parents.
	pub fn ids(&self) -> impl DoubleEndedIterator<Item = NodeId> + use<> {
		(0..u32::try_from(self.nodes.len()).expect("fewer than 2^32 nodes")).map(NodeId)
	}

	pub fn add_folder(&mut self, parent: NodeId, name: impl Into<String>) -> NodeId {
		self.add(parent, name.into(), Entry::Folder(FolderStats::default()))
	}

	pub fn add_file(&mut self, parent: NodeId, name: impl Into<String>, status: FileStatus) -> NodeId {
		self.add(parent, name.into(), Entry::File(status))
	}

	pub fn is_folder(&self, id: NodeId) -> bool {
		matches!(self.node(id).entry, Entry::Folder(_))
	}

	pub fn file(&self, id: NodeId) -> Option<&FileStatus> {
		match &self.node(id).entry {
			Entry::File(status) => Some(status),
			Entry::Folder(_) => None,
		}
	}

	pub fn file_mut(&mut self, id: NodeId) -> Option<&mut FileStatus> {
		match &mut self.node_mut(id).entry {
			Entry::File(status) => Some(status),
			Entry::Folder(_) => None,
		}
	}

	pub fn folder(&self, id: NodeId) -> Option<&FolderStats> {
		match &self.node(id).entry {
			Entry::Folder(stats) => Some(stats),
			Entry::File(_) => None,
		}
	}

	/// Path from the root's first child down to `id`, joined with `/`.
	pub fn relative_path(&self, id: NodeId) -> String {
		let mut names = Vec::new();
		let mut current = Some(id);
		while let Some(node_id) = current {
			let node = self.node(node_id);
			if node.parent.is_some() {
				names.push(node.name.as_str());
			}
			current = node.parent;
		}

		names.reverse();
		names.join("/")
	}

	fn add(&mut self, parent: NodeId, name: String, entry: Entry) -> NodeId {
		let id = NodeId(u32::try_from(self.nodes.len()).expect("fewer than 2^32 nodes"));
		self.nodes.push(Node {
			name,
			parent: Some(parent),
			children: Vec::new(),
			entry,
		});
		self.nodes[parent.index()].children.push(id);
		self.structure_version += 1;
		id
	}
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
pub struct Issue {
	pub file: NodeId,
	/// One of `Failed`, `SourceDamaged`, `Waiting` or `Warning`.
	pub state: FileState,
	pub name: String,
	pub folder: String,
	pub reason: String,
	pub resolution: String,
	pub log: Vec<String>,
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
	pub file: NodeId,
	pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSummary {
	pub reason: String,
	pub resolution: String,
}
