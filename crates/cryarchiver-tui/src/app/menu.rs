use super::tree::Selection;
use super::{Outcome, Ui, ViewMode};
use crate::source::{Action, ArchiveSource, Target as ActionTarget};
use crate::view::{Duplicates, FileId, Issue, IssueId, NodeId, Verb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
	Resolve(IssueId, Verb),
	ShowLog,
	ShowDetails,
	OpenSourceFolder,
	OpenOutputFolder,
	ShowDuplicates,
	ShowOriginal,
	CopyPath,
	ExpandAll,
	CollapseAll,
	RetryFailedIn,
	Rescan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
	Folder(NodeId),
	File { file: FileId, folder: NodeId },
	Issue(IssueId),
}

impl From<Selection> for Target {
	fn from(selection: Selection) -> Self {
		match selection {
			Selection::Folder(folder) => Self::Folder(folder),
			Selection::File { file, folder } => Self::File { file, folder },
		}
	}
}

pub struct Menu {
	pub target: Target,
	pub items: Vec<(MenuItem, String)>,
	pub selected: usize,
	pub anchor: Option<(u16, u16)>,
}

impl Menu {
	pub fn new(source: &dyn ArchiveSource, target: Target, anchor: Option<(u16, u16)>) -> Self {
		let target = match target {
			Target::Issue(id) => issue_by_id(source, id)
				.and_then(|issue| issue.file.zip(issue.folder))
				.map_or(target, |(file, folder)| Target::File { file, folder }),
			other => other,
		};

		let items = match target {
			Target::Folder(_) => folder_items(),
			Target::File { file, .. } => file_items(source, file),
			Target::Issue(id) => issue_items(source, id),
		};

		Self {
			target,
			items,
			selected: 0,
			anchor,
		}
	}
}

fn issue_by_id(source: &dyn ArchiveSource, id: IssueId) -> Option<&Issue> {
	source.view().issues.iter().find(|issue| issue.id == id)
}

fn labelled(item: MenuItem, label: &str) -> (MenuItem, String) {
	(item, label.to_owned())
}

fn folder_items() -> Vec<(MenuItem, String)> {
	vec![
		labelled(MenuItem::ExpandAll, "Expand all"),
		labelled(MenuItem::CollapseAll, "Collapse all"),
		labelled(MenuItem::RetryFailedIn, "Retry failed in folder"),
		labelled(MenuItem::Rescan, "Re-scan source"),
		labelled(MenuItem::OpenSourceFolder, "Open source folder"),
		labelled(MenuItem::OpenOutputFolder, "Open output folder"),
		labelled(MenuItem::CopyPath, "Copy path"),
	]
}

/// The issue's verbs, each under the label its kind gives it.
fn verb_items(issue: &Issue) -> Vec<(MenuItem, String)> {
	[Verb::Retry, Verb::Accept, Verb::Exclude]
		.into_iter()
		.filter_map(|verb| {
			let label = issue.verbs.label(verb)?;
			Some(labelled(MenuItem::Resolve(issue.id, verb), label))
		})
		.collect()
}

fn file_items(source: &dyn ArchiveSource, file: FileId) -> Vec<(MenuItem, String)> {
	let mut items = source
		.view()
		.issues
		.iter()
		.find(|issue| issue.file == Some(file))
		.map(verb_items)
		.unwrap_or_default();

	items.extend([
		labelled(MenuItem::ShowLog, "Show ffmpeg log"),
		labelled(MenuItem::ShowDetails, "Show details"),
		labelled(MenuItem::OpenSourceFolder, "Open source folder"),
		labelled(MenuItem::OpenOutputFolder, "Open output folder"),
	]);

	match source.details(file).map(|details| details.duplicates) {
		Some(Duplicates::PlaceholderOf(_)) => items.push(labelled(MenuItem::ShowOriginal, "Show original")),
		Some(Duplicates::CanonicalOf(_)) => items.push(labelled(MenuItem::ShowDuplicates, "Show duplicates")),
		Some(Duplicates::None) | None => {}
	}

	items.push(labelled(MenuItem::CopyPath, "Copy path"));
	items
}

fn issue_items(source: &dyn ArchiveSource, id: IssueId) -> Vec<(MenuItem, String)> {
	let Some(issue) = issue_by_id(source, id) else {
		return Vec::new();
	};

	let mut items = verb_items(issue);
	if issue.folder.is_some() {
		items.push(labelled(MenuItem::OpenOutputFolder, "Open output folder"));
	}
	items
}

impl Ui {
	pub(super) fn activate(&mut self, source: &dyn ArchiveSource, item: MenuItem, target: Target) -> Outcome {
		let library = &source.view().library;
		let action_target = match target {
			Target::Folder(folder) => Some(ActionTarget::Folder(folder)),
			Target::File { file, .. } => Some(ActionTarget::File(file)),
			Target::Issue(id) => issue_by_id(source, id)
				.and_then(|issue| issue.folder)
				.map(ActionTarget::Folder),
		};

		match (item, target) {
			(MenuItem::Resolve(issue, verb), _) => self.actions.push(Action::Resolve(issue, verb)),
			(MenuItem::RetryFailedIn, Target::Folder(folder)) => self.actions.push(Action::RetryFailedIn(folder)),
			(MenuItem::Rescan, _) => self.actions.push(Action::Rescan),
			(MenuItem::ExpandAll, Target::Folder(folder)) => self.tree.expand_folders_below(library, folder),
			(MenuItem::CollapseAll, Target::Folder(folder)) => self.tree.collapse_below(library, folder),
			(MenuItem::OpenSourceFolder, _) => self.actions.extend(action_target.map(Action::OpenSourceFolder)),
			(MenuItem::OpenOutputFolder, _) => self.actions.extend(action_target.map(Action::OpenOutputFolder)),
			(MenuItem::CopyPath, _) => self.actions.extend(action_target.map(Action::CopyPath)),
			(MenuItem::ShowLog, Target::File { file, folder }) => self.show_log(source, file, folder),
			(MenuItem::ShowDetails | MenuItem::ShowDuplicates, Target::File { file, folder }) => {
				self.open_details(source, folder, file);
			}
			(MenuItem::ShowOriginal, Target::File { file, .. }) => {
				if let Some(Duplicates::PlaceholderOf(link)) = source.details(file).map(|details| details.duplicates) {
					self.reveal_file(source, link.folder, link.file);
					self.mode = ViewMode::Library;
				}
			}
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn show_log(&mut self, source: &dyn ArchiveSource, file: FileId, folder: NodeId) {
		match source.view().issues.iter().position(|issue| issue.file == Some(file)) {
			Some(index) => {
				self.select_issue(index);
				self.mode = ViewMode::Issues;
			}
			None => self.open_details(source, folder, file),
		}
	}
}
