use crate::view::{ArchiveView, FileDetails, NodeId};

/// The TUI's only connection to an archive run. The mock implements it today; the project layer will later.
pub trait ArchiveSource {
	/// Folds in everything that happened since the last call and reports whether the view changed.
	/// The UI thread calls it before every frame, so it must never block on a worker.
	fn refresh(&mut self) -> bool;

	fn view(&self) -> &ArchiveView;

	/// Built on demand while the file details are open, so it may be more expensive than [`Self::view`].
	fn details(&self, file: NodeId) -> Option<FileDetails>;

	fn perform(&mut self, action: Action);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
	TogglePause,
	TogglePerformanceLimit,
	Retry(NodeId),
	RetryFailedIn(NodeId),
	CopyOriginal(NodeId),
	Exclude(NodeId),
	OpenSourceFolder(NodeId),
	OpenOutputFolder(NodeId),
	CopyPath(NodeId),
}
