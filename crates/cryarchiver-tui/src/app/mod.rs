mod keys;
mod menu;
mod mouse;
mod settings;
pub(crate) mod tree;

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyEventKind};
use ratatui::layout::Rect;
use throbber_widgets_tui::ThrobberState;
use tui_input::Input;

pub(crate) use self::menu::Menu;
use self::mouse::Click;
pub(crate) use self::mouse::{Hit, HitMap};
pub(crate) use self::settings::{SETTINGS_ROWS, SettingsRow};
pub use self::tree::Selection;
use self::tree::TreeState;
use crate::source::{Action, ArchiveSource};
use crate::theme::{ColorDepth, Theme};
use crate::view::{FileDetails, FileId, NodeId, Notification};

const TOAST_LIFETIME: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
	Library,
	Details,
	Issues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
	Help,
	Menu,
	Settings,
	ConfirmDelete,
	ConfirmRestart,
}

pub(crate) enum Overlay {
	Help,
	Menu(Menu),
	Settings(usize),
	ConfirmDelete(Input),
	ConfirmRestart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lifecycle {
	Running,
	Quitting,
	Stopping,
}

enum Outcome {
	Ignored,
	Done,
}

/// The whole TUI minus the terminal: feed it events, let it draw frames.
pub struct App<S> {
	source: S,
	ui: Ui,
	generation: u64,
	details: Option<(FileId, u64, Option<FileDetails>)>,
	toast: Option<(Notification, Instant)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
	pub maximized: bool,
	pub log_open: bool,
}

pub(crate) struct Ui {
	pub theme: Theme,
	pub layout: Layout,
	pub mode: ViewMode,
	pub overlay: Option<Overlay>,
	pub search: Option<Input>,
	pub filter: String,
	pub tree: TreeState,
	pub issue_selected: usize,
	/// How many attempts back from the latest the Issues log shows.
	pub attempt_back: usize,
	pub mouse_capture: bool,
	pub lifecycle: Lifecycle,
	pub hits: HitMap,
	pub issues_area: Option<Rect>,
	pub spinner: ThrobberState,
	pub spinner_drawn: bool,
	pub duplicate_cursor: usize,
	pub actions: Vec<Action>,
	last_click: Option<Click>,
}

impl<S: ArchiveSource> App<S> {
	pub fn new(mut source: S, depth: ColorDepth) -> Self {
		let mut tree = TreeState::new();
		let library = &source.view().library;
		tree.set_expanded(library.root(), true);
		for job in &source.view().workers.jobs {
			tree.reveal_folder(library, job.folder);
			tree.set_expanded(job.folder, true);
		}
		if let Some(first) = source.view().workers.jobs.first() {
			tree.reveal_file(library, first.folder, first.file);
		}

		for action in tree.take_actions() {
			source.perform(action);
		}
		tree.sync(&source, "");

		let ui = Ui {
			theme: Theme::dark(depth),
			layout: Layout {
				maximized: false,
				log_open: true,
			},
			mode: ViewMode::Library,
			overlay: None,
			search: None,
			filter: String::new(),
			tree,
			issue_selected: 0,
			attempt_back: 0,
			mouse_capture: true,
			lifecycle: Lifecycle::Running,
			hits: HitMap::default(),
			issues_area: None,
			spinner: ThrobberState::default(),
			spinner_drawn: false,
			duplicate_cursor: 0,
			actions: Vec::new(),
			last_click: None,
		};
		Self {
			source,
			ui,
			generation: 0,
			details: None,
			toast: None,
		}
	}

	pub fn source(&self) -> &S {
		&self.source
	}

	pub fn source_mut(&mut self) -> &mut S {
		&mut self.source
	}

	/// Pulls in the source's changes and notifications; returns whether a redraw is due.
	pub fn refresh(&mut self) -> bool {
		let changed = self.source.refresh();
		if changed {
			self.generation += 1;
		}
		self.take_notifications() || changed
	}

	pub fn advance_spinner(&mut self) {
		self.ui.spinner.calc_next();
	}

	pub fn handle_event(&mut self, event: &Event, now: Instant) {
		self.ui.tree.sync(&self.source, &self.ui.filter);

		match event {
			Event::Key(key) if key.kind == KeyEventKind::Press => {
				self.ui.on_key(&self.source, *key);
			}
			Event::Mouse(mouse) => {
				self.ui.on_mouse(&self.source, *mouse, now);
			}
			Event::Paste(text) => self.ui.on_paste(text),
			_ => {}
		}

		self.perform_pending();
		self.ui.tree.sync(&self.source, &self.ui.filter);
	}

	fn perform_pending(&mut self) {
		let mut actions = std::mem::take(&mut self.ui.actions);
		actions.extend(self.ui.tree.take_actions());
		if actions.is_empty() {
			return;
		}

		for action in actions {
			self.source.perform(action);
		}
		self.generation += 1;
		self.take_notifications();
	}

	fn take_notifications(&mut self) -> bool {
		let Some(latest) = self.source.take_notifications().pop() else {
			return false;
		};

		self.toast = Some((latest, Instant::now()));
		true
	}

	pub fn draw(&mut self, frame: &mut Frame) {
		self.ui.tree.sync(&self.source, &self.ui.filter);
		self.toast = self.toast.take().filter(|(_, at)| at.elapsed() < TOAST_LIFETIME);

		self.refresh_details();
		let details = self.details.as_ref().and_then(|(_, _, details)| details.as_ref());
		let toast = self.toast.as_ref().map(|(notification, _)| notification);
		crate::ui::draw(frame, &self.source, details, toast, &mut self.ui);
	}

	/// Details are rebuilt only when the selected file or the source changed since they were built.
	fn refresh_details(&mut self) {
		let (ViewMode::Details, Some(Selection::File { file, .. })) = (self.ui.mode, self.ui.tree.selected()) else {
			self.details = None;
			return;
		};

		let fresh = self
			.details
			.as_ref()
			.is_some_and(|(id, generation, _)| *id == file && *generation == self.generation);
		if !fresh {
			self.details = Some((file, self.generation, self.source.details(file)));
		}
	}

	pub fn should_quit(&self) -> bool {
		self.ui.lifecycle != Lifecycle::Running
	}

	pub fn mouse_capture(&self) -> bool {
		self.ui.mouse_capture
	}

	pub fn set_mouse_capture(&mut self, captured: bool) {
		self.ui.mouse_capture = captured;
	}

	/// Whether the last frame showed the scanning spinner, so the runner only animates what is visible.
	pub fn spinner_visible(&self) -> bool {
		self.ui.spinner_drawn
	}

	pub fn begin_shutdown(&mut self) {
		self.ui.lifecycle = Lifecycle::Stopping;
		self.ui.overlay = None;
	}

	pub fn shutdown(&mut self) {
		self.source.shutdown();
	}

	pub fn maximized(&self) -> bool {
		self.ui.layout.maximized
	}

	pub fn log_open(&self) -> bool {
		self.ui.layout.log_open
	}

	pub fn mode(&self) -> ViewMode {
		self.ui.mode
	}

	pub fn overlay(&self) -> Option<OverlayKind> {
		self.ui.overlay.as_ref().map(|overlay| match overlay {
			Overlay::Help => OverlayKind::Help,
			Overlay::Menu(_) => OverlayKind::Menu,
			Overlay::Settings(_) => OverlayKind::Settings,
			Overlay::ConfirmDelete(_) => OverlayKind::ConfirmDelete,
			Overlay::ConfirmRestart => OverlayKind::ConfirmRestart,
		})
	}

	pub fn menu_items(&self) -> Vec<String> {
		match &self.ui.overlay {
			Some(Overlay::Menu(menu)) => menu.items.iter().map(|(_, label)| label.clone()).collect(),
			_ => Vec::new(),
		}
	}

	pub fn searching(&self) -> bool {
		self.ui.search.is_some()
	}

	pub fn filter(&self) -> &str {
		&self.ui.filter
	}

	pub fn selected(&self) -> Option<Selection> {
		self.ui.tree.selected()
	}

	/// The name of the selected folder or file, as the Library shows it.
	pub fn selected_name(&self) -> Option<String> {
		let row = self.ui.tree.rows().get(self.ui.tree.selected_index()?)?;
		match row.kind {
			tree::RowKind::Folder(folder, _) => Some(self.source.view().library.node(folder).name.clone()),
			kind => tree::file_row(&self.source, kind).map(|file| file.name.clone()),
		}
	}

	pub fn visible_rows(&self) -> usize {
		self.ui.tree.rows().len()
	}

	pub fn selected_issue(&self) -> usize {
		self.ui.issue_selected
	}

	pub fn toast(&self) -> Option<&Notification> {
		self.toast.as_ref().map(|(notification, _)| notification)
	}
}

impl Ui {
	fn open_menu(&mut self, source: &dyn ArchiveSource, anchor: Option<(u16, u16)>) -> Outcome {
		let target = match self.mode {
			ViewMode::Issues => source
				.view()
				.issues
				.get(self.issue_selected)
				.map(|issue| menu::Target::Issue(issue.id)),
			ViewMode::Library | ViewMode::Details => self.tree.selected().map(menu::Target::from),
		};
		let Some(target) = target else {
			return Outcome::Ignored;
		};

		self.overlay = Some(Overlay::Menu(Menu::new(source, target, anchor)));
		Outcome::Done
	}

	fn set_filter(&mut self, filter: String) {
		if filter != self.filter {
			self.filter.clone_from(&filter);
			self.actions.push(Action::SetFilter(filter));
		}
	}

	fn reveal_file(&mut self, source: &dyn ArchiveSource, folder: NodeId, file: FileId) {
		let visible = self
			.tree
			.rows()
			.iter()
			.any(|row| matches!(row.kind, tree::RowKind::File { file: id, .. } if id == file));
		if !self.filter.is_empty() && !visible {
			self.set_filter(String::new());
		}

		self.tree.reveal_file(&source.view().library, folder, file);
	}

	fn open_details(&mut self, source: &dyn ArchiveSource, folder: NodeId, file: FileId) {
		self.reveal_file(source, folder, file);
		self.mode = ViewMode::Details;
	}

	fn escape(&mut self) -> Outcome {
		if self.overlay.take().is_some() {
			return Outcome::Done;
		}

		if self.mode != ViewMode::Library {
			self.mode = ViewMode::Library;
		} else if !self.filter.is_empty() {
			self.set_filter(String::new());
		} else if self.layout.maximized {
			self.layout.maximized = false;
		} else {
			return Outcome::Ignored;
		}
		Outcome::Done
	}

	fn select_issue(&mut self, index: usize) {
		if index != self.issue_selected {
			self.attempt_back = 0;
		}
		self.issue_selected = index;
	}
}
