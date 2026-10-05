use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use super::tree::{Expansion, RowKind, Selection};
use super::{DuplicateCycle, Lifecycle, Outcome, Overlay, Ui, ViewMode};
use crate::source::{Action, ArchiveSource};
use crate::view::{Duplicates, FileId, IssueVerbs, RunState, Verb};

impl Ui {
	/// `burst` marks a key that belongs to a paste delivered as key presses: it may only type into a text field.
	pub(super) fn on_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent, burst: bool) -> Outcome {
		if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
			self.lifecycle = Lifecycle::Quitting;
			return Outcome::Done;
		}

		let typing = self.search.is_some()
			|| matches!(
				self.overlay,
				Some(Overlay::ConfirmDelete(_) | Overlay::EditQuality { .. })
			);
		if burst && (!typing || key.code == KeyCode::Enter) {
			return Outcome::Ignored;
		}

		if self.search.is_some() {
			return self.on_search_key(source, key);
		}

		if self.overlay.is_some() {
			return self.on_overlay_key(source, key);
		}

		match self.on_global_key(source, key) {
			Outcome::Ignored => {}
			handled @ Outcome::Done => return handled,
		}

		if self.layout.log_maximized {
			return self.on_log_key(source, key);
		}

		match self.mode {
			ViewMode::Library => self.on_library_key(source, key),
			ViewMode::Details => self.on_details_key(source, key),
			ViewMode::Issues => self.on_issues_key(source, key),
		}
	}

	/// Bracketed paste only reaches text inputs, so pasted text never fires shortcuts.
	pub(super) fn on_paste(&mut self, source: &dyn ArchiveSource, text: &str) {
		let ((Some(input), _) | (None, Some(Overlay::ConfirmDelete(input) | Overlay::EditQuality { input, .. }))) =
			(&mut self.search, &mut self.overlay)
		else {
			return;
		};

		for c in text.chars().filter(|c| !c.is_control()) {
			input.handle(tui_input::InputRequest::InsertChar(c));
		}

		if let Some(query) = self.search.as_ref().map(|input| input.value().to_owned()) {
			self.set_filter(&source.view().library, query);
		}
	}

	fn on_search_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let library = &source.view().library;
		match key.code {
			KeyCode::Esc => {
				self.search = None;
				self.set_filter(library, String::new());
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
				let query = input.value().to_owned();
				self.set_filter(library, query);
			}
		}
		Outcome::Done
	}

	fn on_overlay_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		match &mut self.overlay {
			Some(Overlay::Menu(menu)) => match key.code {
				KeyCode::Up | KeyCode::Char('k') => menu.selected = menu.selected.saturating_sub(1),
				KeyCode::Down | KeyCode::Char('j') => menu.selected = (menu.selected + 1).min(menu.items.len() - 1),
				KeyCode::Enter => {
					let (item, target) = (menu.items[menu.selected].0, menu.target);
					self.overlay = None;
					return self.activate(source, item, target);
				}
				KeyCode::Esc | KeyCode::Char('m') => self.overlay = None,
				KeyCode::Char('q') => self.quit(source),
				KeyCode::F(2) => self.mouse_capture = !self.mouse_capture,
				_ => {}
			},
			Some(Overlay::Settings(selected)) => {
				let selected = *selected;
				return self.on_settings_key(source, key, selected);
			}
			Some(Overlay::EditQuality { .. }) => return self.on_edit_quality_key(source, key),
			Some(Overlay::ConfirmDelete(_)) => return self.on_confirm_delete_key(key),
			Some(Overlay::ConfirmRestart) => return self.on_confirm_restart_key(key),
			Some(Overlay::ConfirmQuit) => return self.on_confirm_quit_key(key),
			Some(Overlay::Help) | None => match key.code {
				KeyCode::Char('q') => self.quit(source),
				KeyCode::F(2) => self.mouse_capture = !self.mouse_capture,
				KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => self.overlay = None,
				_ => {}
			},
		}
		Outcome::Done
	}

	pub(super) fn quit(&mut self, source: &dyn ArchiveSource) {
		let view = source.view();
		let idle = matches!(
			view.header.run_state,
			RunState::Paused | RunState::Watching | RunState::Finished
		);
		if idle && view.workers.jobs.is_empty() {
			self.lifecycle = Lifecycle::Quitting;
		} else {
			self.overlay = Some(Overlay::ConfirmQuit);
		}
	}

	fn on_confirm_quit_key(&mut self, key: KeyEvent) -> Outcome {
		match key.code {
			KeyCode::Enter | KeyCode::Char('y') => self.lifecycle = Lifecycle::Quitting,
			KeyCode::Esc | KeyCode::Char('n' | 'q') => self.overlay = None,
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn on_global_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		match key.code {
			KeyCode::Char('q') => self.quit(source),
			KeyCode::F(2) => self.mouse_capture = !self.mouse_capture,
			KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
			KeyCode::Char('s') => self.overlay = Some(Overlay::Settings(0)),
			KeyCode::Char('p') => {
				let paused = source.view().header.run_state == RunState::Paused;
				self.actions.push(if paused { Action::Start } else { Action::Pause });
			}
			KeyCode::Char('e') => self.toggle_limit(source),
			KeyCode::Char('z') => {
				self.layout.maximized = !self.layout.maximized;
				self.layout.log_maximized = false;
			}
			KeyCode::Char('L') => {
				self.layout.log_maximized = !self.layout.log_maximized;
				self.layout.maximized = false;
			}
			KeyCode::Char('l') if self.layout.log_maximized => {
				self.layout.log_maximized = false;
				self.layout.log_open = false;
			}
			KeyCode::Char('l') => self.layout.log_open = !self.layout.log_open,
			KeyCode::Char('/') => {
				self.layout.log_maximized = false;
				self.mode = ViewMode::Library;
				self.search = Some(Input::new(self.filter.clone()));
			}
			KeyCode::Char('i') if self.layout.log_maximized => {
				self.layout.log_maximized = false;
				self.mode = ViewMode::Issues;
			}
			KeyCode::Char('i') => {
				self.mode = if self.mode == ViewMode::Issues {
					ViewMode::Library
				} else {
					ViewMode::Issues
				};
			}
			KeyCode::Char('m') => {
				self.layout.log_maximized = false;
				return self.open_menu(source, None);
			}
			KeyCode::Esc => return self.escape(source),
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn on_log_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let log = &source.view().log;
		match key.code {
			KeyCode::Up => self.log_scroll.scroll_by(log, -1),
			KeyCode::Down => self.log_scroll.scroll_by(log, 1),
			KeyCode::PageUp => self.log_scroll.page_by(log, -1),
			KeyCode::PageDown => self.log_scroll.page_by(log, 1),
			KeyCode::Home => self.log_scroll.jump_to_newest(),
			KeyCode::End => self.log_scroll.jump_to_oldest(log),
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn toggle_limit(&mut self, source: &dyn ArchiveSource) {
		const DEFAULT_CAP: u8 = 25;

		let cap = match source.view().settings.performance_limit {
			Some(_) => None,
			None => Some(DEFAULT_CAP),
		};
		self.actions
			.push(Action::ChangeSettings(crate::source::SettingsChange::PerformanceLimit(
				cap,
			)));
	}

	fn on_library_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let selection = self.tree.selected();

		match (key.code, selection) {
			(KeyCode::Up | KeyCode::Char('k'), _) => self.tree.move_by(-1),
			(KeyCode::Down | KeyCode::Char('j'), _) => self.tree.move_by(1),
			(KeyCode::PageUp, _) => self.tree.page_by(-1),
			(KeyCode::PageDown, _) => self.tree.page_by(1),
			(KeyCode::Home, _) => self.tree.select_index(0),
			(KeyCode::End, _) => self.tree.select_last(),
			(KeyCode::Left, _) => self.collapse_or_parent(source),
			(KeyCode::Right, Some(Selection::Folder(folder))) => self.expand_or_child(folder),
			(KeyCode::Enter, Some(Selection::Folder(folder))) if self.filter.is_empty() => self.tree.toggle(folder),
			(KeyCode::Enter, Some(Selection::File { .. })) => self.mode = ViewMode::Details,
			(KeyCode::Char('r'), Some(Selection::Folder(folder))) => self.actions.push(Action::RetryFailedIn(folder)),
			(KeyCode::Char('r'), Some(Selection::File { file, .. })) => {
				return self.resolve_file(source, file, Verb::Retry);
			}
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn resolve_file(&mut self, source: &dyn ArchiveSource, file: FileId, verb: Verb) -> Outcome {
		let issue = source.view().issues.iter().find(|issue| issue.file == Some(file));
		match issue.filter(|issue| issue.verbs.label(verb).is_some()) {
			Some(issue) => {
				self.actions.push(Action::Resolve(issue.id, verb));
				Outcome::Done
			}
			None => Outcome::Ignored,
		}
	}

	fn collapse_or_parent(&mut self, source: &dyn ArchiveSource) {
		let library = &source.view().library;
		let parent = match self.tree.selected() {
			Some(Selection::Folder(folder)) if self.filter.is_empty() && self.tree.expansion(folder).is_some() => {
				self.tree.set_expanded(folder, false);
				return;
			}
			Some(Selection::Folder(folder)) => library.node(folder).parent,
			Some(Selection::File { folder, .. }) => Some(folder),
			None => None,
		};

		let Some(parent) = parent else { return };
		let index = self
			.tree
			.rows()
			.iter()
			.position(|row| matches!(row.kind, RowKind::Folder(id, _) if id == parent));
		if let Some(index) = index {
			self.tree.select_index(index);
		}
	}

	fn expand_or_child(&mut self, folder: crate::view::NodeId) {
		if self.tree.expansion(folder) == Some(Expansion::Full) || !self.filter.is_empty() {
			self.tree.move_by(1);
		} else {
			self.tree.set_expanded(folder, true);
		}
	}

	fn on_details_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let file = match self.tree.selected() {
			Some(Selection::File { file, .. }) => Some(file),
			_ => None,
		};

		match (key.code, file) {
			(KeyCode::Enter | KeyCode::Backspace, _) => self.mode = ViewMode::Library,
			(KeyCode::Up | KeyCode::Char('k'), _) => self.tree.select_file_step(false),
			(KeyCode::Down | KeyCode::Char('j'), _) => self.tree.select_file_step(true),
			(KeyCode::Char('g'), Some(file)) => return self.jump_to_duplicate(source, file),
			(KeyCode::Char('r'), Some(file)) => return self.resolve_file(source, file, Verb::Retry),
			(KeyCode::Char('c'), Some(file)) => return self.resolve_file(source, file, Verb::Accept),
			(KeyCode::Char('x'), Some(file)) => return self.resolve_file(source, file, Verb::Exclude),
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn jump_to_duplicate(&mut self, source: &dyn ArchiveSource, file: FileId) -> Outcome {
		let target = match source.details(file).map(|details| details.duplicates) {
			Some(Duplicates::PlaceholderOf(link)) => {
				self.duplicate_cycle = self
					.duplicate_cycle
					.filter(|cycle| cycle.original == link.file)
					.map(|cycle| DuplicateCycle {
						landed: link.file,
						..cycle
					});
				link
			}
			Some(Duplicates::CanonicalOf(links)) if !links.is_empty() => {
				let next = self
					.duplicate_cycle
					.filter(|cycle| cycle.original == file)
					.map_or(0, |cycle| cycle.next);
				let link = links[next % links.len()].clone();
				self.duplicate_cycle = Some(DuplicateCycle {
					original: file,
					next: next + 1,
					landed: link.file,
				});
				link
			}
			_ => return Outcome::Ignored,
		};

		self.reveal_file(source, target.folder, target.file);
		Outcome::Done
	}

	fn on_issues_key(&mut self, source: &dyn ArchiveSource, key: KeyEvent) -> Outcome {
		let issues = &source.view().issues;
		let last = issues.len().saturating_sub(1);
		self.issue_selected = self.issue_selected.min(last);
		let issue = issues.get(self.issue_selected);
		let attempts = issue.map_or(0, |issue| usize::try_from(issue.attempts).unwrap_or(usize::MAX));

		match key.code {
			KeyCode::Up | KeyCode::Char('k') => self.select_issue(self.issue_selected.saturating_sub(1)),
			KeyCode::Down | KeyCode::Char('j') => self.select_issue((self.issue_selected + 1).min(last)),
			KeyCode::Home => self.select_issue(0),
			KeyCode::End => self.select_issue(last),
			KeyCode::Left => self.attempt_back = (self.attempt_back + 1).min(attempts.saturating_sub(1)),
			KeyCode::Right => self.attempt_back = self.attempt_back.saturating_sub(1),
			KeyCode::Enter => match issue.and_then(|issue| issue.file.zip(issue.folder)) {
				Some((file, folder)) => self.open_details(source, folder, file),
				None => return Outcome::Ignored,
			},
			KeyCode::Char(key) => return self.resolve_selected_issue(issue.map(|issue| (issue.id, &issue.verbs)), key),
			_ => return Outcome::Ignored,
		}
		Outcome::Done
	}

	fn resolve_selected_issue(&mut self, issue: Option<(crate::view::IssueId, &IssueVerbs)>, key: char) -> Outcome {
		let verb = match key {
			'r' => Verb::Retry,
			'c' => Verb::Accept,
			'x' => Verb::Exclude,
			_ => return Outcome::Ignored,
		};

		match issue.filter(|(_, verbs)| verbs.label(verb).is_some()) {
			Some((id, _)) => {
				self.actions.push(Action::Resolve(id, verb));
				Outcome::Done
			}
			None => Outcome::Ignored,
		}
	}
}
