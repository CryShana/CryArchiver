use super::{Outcome, Ui, ViewMode};
use crate::source::{Action, ArchiveSource};
use crate::view::{Duplicates, NodeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
	Retry,
	CopyOriginal,
	Exclude,
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
}

impl MenuItem {
	pub const fn label(self) -> &'static str {
		match self {
			Self::Retry => "Retry",
			Self::CopyOriginal => "Copy original",
			Self::Exclude => "Exclude",
			Self::ShowLog => "Show ffmpeg log",
			Self::ShowDetails => "Show details",
			Self::OpenSourceFolder => "Open source folder",
			Self::OpenOutputFolder => "Open output folder",
			Self::ShowDuplicates => "Show duplicates",
			Self::ShowOriginal => "Show original",
			Self::CopyPath => "Copy path",
			Self::ExpandAll => "Expand all",
			Self::CollapseAll => "Collapse all",
			Self::RetryFailedIn => "Retry failed in folder",
		}
	}
}

pub struct Menu {
	pub target: NodeId,
	pub items: Vec<MenuItem>,
	pub selected: usize,
	pub anchor: Option<(u16, u16)>,
}

impl Menu {
	pub fn for_node(source: &dyn ArchiveSource, target: NodeId, anchor: Option<(u16, u16)>) -> Self {
		let view = source.view();
		let items = if view.library.is_folder(target) {
			vec![
				MenuItem::ExpandAll,
				MenuItem::CollapseAll,
				MenuItem::RetryFailedIn,
				MenuItem::OpenSourceFolder,
				MenuItem::OpenOutputFolder,
				MenuItem::CopyPath,
			]
		} else {
			file_items(source, target)
		};

		Self {
			target,
			items,
			selected: 0,
			anchor,
		}
	}
}

fn file_items(source: &dyn ArchiveSource, file: NodeId) -> Vec<MenuItem> {
	let mut items = Vec::new();
	if source.view().issues.iter().any(|issue| issue.file == file) {
		items.extend([MenuItem::Retry, MenuItem::CopyOriginal, MenuItem::Exclude]);
	}

	items.extend([
		MenuItem::ShowLog,
		MenuItem::ShowDetails,
		MenuItem::OpenSourceFolder,
		MenuItem::OpenOutputFolder,
	]);

	match source.details(file).map(|details| details.duplicates) {
		Some(Duplicates::PlaceholderOf(_)) => items.push(MenuItem::ShowOriginal),
		Some(Duplicates::CanonicalOf(_)) => items.push(MenuItem::ShowDuplicates),
		Some(Duplicates::None) | None => {}
	}

	items.push(MenuItem::CopyPath);
	items
}

impl Ui {
	pub(super) fn activate(&mut self, source: &dyn ArchiveSource, item: MenuItem, target: NodeId) -> Outcome {
		let library = &source.view().library;
		match item {
			MenuItem::Retry => return Outcome::Perform(Action::Retry(target)),
			MenuItem::CopyOriginal => return Outcome::Perform(Action::CopyOriginal(target)),
			MenuItem::Exclude => return Outcome::Perform(Action::Exclude(target)),
			MenuItem::RetryFailedIn => return Outcome::Perform(Action::RetryFailedIn(target)),
			MenuItem::OpenSourceFolder => return Outcome::Perform(Action::OpenSourceFolder(target)),
			MenuItem::OpenOutputFolder => return Outcome::Perform(Action::OpenOutputFolder(target)),
			MenuItem::CopyPath => return Outcome::Perform(Action::CopyPath(target)),
			MenuItem::ShowLog => self.show_log(source, target),
			MenuItem::ShowDetails | MenuItem::ShowDuplicates => self.open_details(source, target),
			MenuItem::ShowOriginal => {
				if let Some(Duplicates::PlaceholderOf(link)) = source.details(target).map(|details| details.duplicates)
				{
					self.reveal(source, link.file);
					self.mode = ViewMode::Library;
				}
			}
			MenuItem::ExpandAll => self.tree.set_expanded_below(library, target, true),
			MenuItem::CollapseAll => self.tree.set_expanded_below(library, target, false),
		}
		Outcome::Done
	}

	fn show_log(&mut self, source: &dyn ArchiveSource, file: NodeId) {
		match source.view().issues.iter().position(|issue| issue.file == file) {
			Some(index) => {
				self.issue_selected = index;
				self.mode = ViewMode::Issues;
			}
			None => self.open_details(source, file),
		}
	}
}
