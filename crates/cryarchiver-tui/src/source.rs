use std::time::Duration;

use crate::view::{
	ArchiveView, BluRay, FileDetails, FileId, FileRow, Filter, FilterResult, IssueId, NodeId, Notification, Overrides,
	Speed, Verb,
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

	/// Changes whenever a folder's file rows are loaded, released, inserted or removed, and whenever a new filter
	/// result replaces the old one. Status changes are patched in place, in the `files` slices and in the filter
	/// result's rows alike, and never bump it; the TUI reads statuses afresh every frame.
	fn rows_version(&self) -> u64;

	/// The result of the last [`Action::SetFilter`] with a non-empty filter, once it is ready. While the filter has a
	/// state, the result follows the run, and a file joining or leaving it bumps [`Self::rows_version`].
	fn filter(&self) -> Option<&FilterResult>;

	/// Built on demand while the file details are open, so it may be more expensive than [`Self::view`].
	fn details(&self, file: FileId) -> Option<FileDetails>;

	/// The failure log of one attempt, numbered from 1 up to [`crate::view::Issue::attempts`]. `None` while it is
	/// still loading. The TUI asks only for the attempt on screen and caches the answer.
	fn issue_log(&self, issue: IssueId, attempt: u32) -> Option<Vec<String>>;

	/// Notifications raised since the last call, oldest first. Log lines stay in [`ArchiveView::log`].
	fn take_notifications(&mut self) -> Vec<Notification>;

	/// Text a "Copy path" asked to put on the clipboard since the last call. The terminal runner hands it to the
	/// terminal.
	fn take_clipboard(&mut self) -> Option<String> {
		None
	}

	fn perform(&mut self, action: Action);

	/// Called once on every exit of a started TUI, while a "stopping" frame is on screen when there is a terminal.
	/// It may block until running jobs are cancelled and requeued, which can take seconds. A source that is dropped
	/// without it, because the TUI refused to start, must still stop its run safely in `Drop`.
	fn shutdown(&mut self);
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
	Start,
	Pause,
	StopNow,
	Rescan,
	RestartPendingJobs,
	ChangeSettings(SettingsChange),
	Resolve(IssueId, Verb),
	RetryFailedIn(NodeId),
	/// Archives again the files whose original was kept: the file, or every one at any depth in the folder.
	RetryKept(Target),
	/// The folder was opened or closed in the Library, so its file rows can be loaded or released.
	Expanded(NodeId, bool),
	/// An empty filter clears it.
	SetFilter(Filter),
	OpenSourceFolder(Target),
	OpenOutputFolder(Target),
	CopyPath(Target),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
	Folder(NodeId),
	File(FileId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingsChange {
	Speed(Speed),
	/// Replaces every override; `Overrides::default()` returns to the preset. The source validates and normalizes.
	Overrides(Overrides),
	MinSavings(u8),
	Watch(bool),
	PerformanceLimit(Option<u8>),
	StabilityWindow(Duration),
	KeepAuxiliaryImages(bool),
	BluRay(BluRay),
	/// Turning deletion on carries the text the user typed; the source refuses it unless it reads `DELETE`.
	DeleteOriginals {
		enabled: bool,
		confirmation: String,
	},
}
