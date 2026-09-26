use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::{Lifecycle, Outcome, Overlay, Ui, ViewMode};
use crate::source::{Action, ArchiveSource};
use crate::view::Duplicates;

impl Ui {
	pub(super) fn on_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
			self.lifecycle = Lifecycle::Quitting;
			return Outcome::Done;
		}

		if self.search.is_some() {
			return self.on_search_key(source, key);
		}

		if self.overlay.is_some() {
			return self.on_overlay_key(source, key);
		}

		match self.on_global_key(source, key) {
			Outcome::Ignored => {}
			handled => return handled,
		}

		match self.mode {
			ViewMode::Library => self.on_library_key(source, key),
			ViewMode::Details => self.on_details_key(source, key),
			ViewMode::Issues => self.on_issues_key(source, key),
		}
	}

	pub(super) fn on_paste(&mut self, source: &dyn ArchiveSource, text: &str) -> Outcome {
		let Some(input) = &mut self.search else {
			return Outcome::Ignored;
		};

		for c in text.chars().filter(|c| !c.is_control()) {
			input.handle(tui_input::InputRequest::InsertChar(c));
		}
		self.filter = input.value().to_owned();
		self.tree.sync(&source.view().library, &self.filter);
		Outcome::Done
	}

	fn on_search_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		match key.code {
			KeyCode::Esc => {
				self.search = None;
				self.filter.clear();
			}
			KeyCode::Enter => self.search = None,
			KeyCode::Up => self.tree.move_by(-1),
			KeyCode::Down => self.tree.move_by(1),
			KeyCode::PageUp => self.tree.page_by(-1),
			KeyCode::PageDown => self.tree.page_by(1),
			_ => {
				let Some(input) = &mut self.search else {
					return Outcome::Ignored;
				};
				input.handle_event(&Event::Key(key));
				self.filter = input.value().to_owned();
			}
		}

		self.tree.sync(&source.view().library, &self.filter);
		Outcome::Done
	}

	fn on_overlay_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let Some(Overlay::Menu(menu)) = &mut self.overlay else {
			match key.code {
				KeyCode::Char('q') => self.lifecycle = Lifecycle::Quitting,
				KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => self.overlay = None,
				_ => {}
			}
			return Outcome::Done;
		};

		match key.code {
			KeyCode::Up | KeyCode::Char('k') => menu.selected = menu.selected.saturating_sub(1),
			KeyCode::Down | KeyCode::Char('j') => menu.selected = (menu.selected + 1).min(menu.items.len() - 1),
			KeyCode::Enter => {
				let (item, target) = (menu.items[menu.selected], menu.target);
				self.overlay = None;
				return self.activate(source, item, target);
			}
			KeyCode::Esc | KeyCode::Char('m') => self.overlay = None,
			KeyCode::Char('q') => self.lifecycle = Lifecycle::Quitting,
			_ => {}
		}
		Outcome::Done
	}

	fn on_global_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		match key.code {
			KeyCode::Char('q') => self.lifecycle = Lifecycle::Quitting,
			KeyCode::F(2) => self.mouse_capture = !self.mouse_capture,
			KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
			KeyCode::Char('p') => return Outcome::Perform(Action::TogglePause),
			KeyCode::Char('e') => return Outcome::Perform(Action::TogglePerformanceLimit),
			KeyCode::Char('z') => self.layout.maximized = !self.layout.maximized,
			KeyCode::Char('l') => self.layout.log_open = !self.layout.log_open,
			KeyCode::Char('/') => {
				self.mode = ViewMode::Library;
				self.search = Some(Input::new(self.filter.clone()));
			}
			KeyCode::Char('i') => {
				self.mode = if self.mode == ViewMode::Issues {
					ViewMode::Library
				} else {
					ViewMode::Issues
				};
			}
			KeyCode::Char('m') => return self.open_menu(source, None),
			KeyCode::Esc => return self.escape(),
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn on_library_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let library = &source.view().library;
		let Some(selected) = self.tree.selected() else {
			return Outcome::Ignored;
		};

		match key.code {
			KeyCode::Up | KeyCode::Char('k') => self.tree.move_by(-1),
			KeyCode::Down | KeyCode::Char('j') => self.tree.move_by(1),
			KeyCode::PageUp => self.tree.page_by(-1),
			KeyCode::PageDown => self.tree.page_by(1),
			KeyCode::Home => self.tree.select_index(0),
			KeyCode::End => self.tree.select_last(),
			KeyCode::Left => self.collapse_or_parent(source),
			KeyCode::Right => self.expand_or_child(source),
			KeyCode::Enter if library.is_folder(selected) => self.tree.toggle(selected),
			KeyCode::Enter => self.mode = ViewMode::Details,
			KeyCode::Char('r') if library.is_folder(selected) => {
				return Outcome::Perform(Action::RetryFailedIn(selected));
			}
			KeyCode::Char('r') => return Outcome::Perform(Action::Retry(selected)),
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn collapse_or_parent(&mut self, source: &dyn ArchiveSource) {
		let library = &source.view().library;
		let Some(selected) = self.tree.selected() else { return };
		let open_folder = self.tree.rows().iter().any(|row| row.id == selected && row.open);

		if open_folder && self.filter.is_empty() {
			self.tree.set_expanded(selected, false);
		} else if let Some(parent) = library.node(selected).parent
			&& let Some(index) = self.tree.rows().iter().position(|row| row.id == parent)
		{
			self.tree.select_index(index);
		}
	}

	fn expand_or_child(&mut self, source: &dyn ArchiveSource) {
		let library = &source.view().library;
		let Some(selected) = self.tree.selected() else { return };
		if !library.is_folder(selected) {
			return;
		}

		if self.tree.is_expanded(selected) || !self.filter.is_empty() {
			self.tree.move_by(1);
		} else {
			self.tree.set_expanded(selected, true);
		}
	}

	fn on_details_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let library = &source.view().library;
		let selected = self.tree.selected();

		match (key.code, selected) {
			(KeyCode::Enter | KeyCode::Backspace, _) => self.mode = ViewMode::Library,
			(KeyCode::Up | KeyCode::Char('k'), _) => self.tree.select_file_step(library, false),
			(KeyCode::Down | KeyCode::Char('j'), _) => self.tree.select_file_step(library, true),
			(KeyCode::Char('g'), Some(file)) => return self.jump_to_duplicate(source, file),
			(KeyCode::Char('r'), Some(file)) => return Outcome::Perform(Action::Retry(file)),
			(KeyCode::Char('c'), Some(file)) => return Outcome::Perform(Action::CopyOriginal(file)),
			(KeyCode::Char('x'), Some(file)) => return Outcome::Perform(Action::Exclude(file)),
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn jump_to_duplicate(&mut self, source: &dyn ArchiveSource, file: crate::view::NodeId) -> Outcome {
		let target = match source.details(file).map(|details| details.duplicates) {
			Some(Duplicates::PlaceholderOf(link)) => link.file,
			Some(Duplicates::CanonicalOf(links)) if !links.is_empty() => {
				let link = &links[self.duplicate_cursor % links.len()];
				self.duplicate_cursor += 1;
				link.file
			}
			_ => return Outcome::Ignored,
		};

		self.reveal(source, target);
		Outcome::Done
	}

	fn on_issues_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let issues = &source.view().issues;
		let last = issues.len().saturating_sub(1);
		self.issue_selected = self.issue_selected.min(last);
		let file = issues.get(self.issue_selected).map(|issue| issue.file);

		match (key.code, file) {
			(KeyCode::Up | KeyCode::Char('k'), _) => self.issue_selected = self.issue_selected.saturating_sub(1),
			(KeyCode::Down | KeyCode::Char('j'), _) => self.issue_selected = (self.issue_selected + 1).min(last),
			(KeyCode::Home, _) => self.issue_selected = 0,
			(KeyCode::End, _) => self.issue_selected = last,
			(KeyCode::Enter, Some(file)) => self.open_details(source, file),
			(KeyCode::Char('r'), Some(file)) => return Outcome::Perform(Action::Retry(file)),
			(KeyCode::Char('c'), Some(file)) => return Outcome::Perform(Action::CopyOriginal(file)),
			(KeyCode::Char('x'), Some(file)) => return Outcome::Perform(Action::Exclude(file)),
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}
}
