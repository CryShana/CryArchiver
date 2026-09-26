use std::time::Duration;

use crate::view::{
	ArchiveView, FileDetails, FileId, FileRow, FilterResult, IssueId, NodeId, Notification, Speed, Verb,
};

/// Everything the TUI reads from an archive run and every command it sends back (project layer spec §11.3).
pub trait ArchiveSource {
	/// Folds in everything that happened since the last call and reports whether the view changed.
	/// The UI thread calls it before every frame, so it must never block on a worker.
	fn refresh(&mut self) -> bool;

	fn view(&self) -> &ArchiveView;

	/// The file rows of a folder the TUI has expanded with [`Action::Expanded`]. `None` while they are still loading,
	/// or for a folder that isn't expanded.
	fn files(&self, folder: NodeId) -> Option<&[FileRow]>;

	/// Changes whenever a folder's file rows are loaded, released or gain or lose rows, and whenever the filter
	/// result changes. Status changes inside rows don't bump it.
	fn rows_version(&self) -> u64;

	/// The result of the last [`Action::SetFilter`] with a non-empty query, once it is ready.
	fn filter(&self) -> Option<&FilterResult>;

	/// Built on demand while the file details are open, so it may be more expensive than [`Self::view`].
	fn details(&self, file: FileId) -> Option<FileDetails>;

	/// Notifications raised since the last call, oldest first. Log lines stay in [`ArchiveView::log`].
	fn take_notifications(&mut self) -> Vec<Notification>;

	fn perform(&mut self, action: Action);

	/// Called once when the TUI quits, while a "stopping" frame is on screen. It may block until running jobs are
	/// cancelled and requeued, which can take seconds.
	fn shutdown(&mut self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
	Start,
	Pause,
	StopNow,
	Rescan,
	RestartPendingJobs,
	ChangeSettings(SettingsChange),
	Resolve(IssueId, Verb),
	RetryFailedIn(NodeId),
	/// The folder was opened or closed in the Library, so its file rows can be loaded or released.
	Expanded(NodeId, bool),
	/// An empty query clears the filter.
	SetFilter(String),
	OpenSourceFolder(Target),
	OpenOutputFolder(Target),
	CopyPath(Target),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
	Folder(NodeId),
	File(FileId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsChange {
	Speed(Speed),
	MinSavings(u8),
	Watch(bool),
	PerformanceLimit(Option<u8>),
	StabilityWindow(Duration),
	KeepAuxiliaryImages(bool),
	/// Turning deletion on carries the text the user typed; the source refuses it unless it reads `DELETE`.
	DeleteOriginals {
		enabled: bool,
		confirmation: String,
	},
}
