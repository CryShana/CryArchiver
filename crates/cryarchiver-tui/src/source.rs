use crate::view::{ArchiveView, FileDetails, NodeId};

/// Everything the TUI reads from an archive run and every command it sends back.
pub trait ArchiveSource {
	/// Folds in everything that happened since the last call and reports whether the view changed.
	/// The UI thread calls it before every frame, so it must never block on a worker.
	fn refresh(&mut self) -> bool;

	fn view(&self) -> &ArchiveView;

	/// Built on demand while the file details are open, so it may be more expensive than [`Self::view`].
	fn details(&self, file: NodeId) -> Option<FileDetails>;

	fn perform(&mut self, action: Action);

	/// Called once when the TUI quits, while a "stopping" frame is on screen. It may block until running jobs are
	/// cancelled and requeued, which can take seconds.
	fn shutdown(&mut self);
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
