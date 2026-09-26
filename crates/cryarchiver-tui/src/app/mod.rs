mod keys;
mod menu;
mod mouse;
pub(crate) mod tree;

use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyEventKind};
use ratatui::layout::Rect;
use throbber_widgets_tui::ThrobberState;
use tui_input::Input;

pub(crate) use self::menu::Menu;
use self::mouse::Click;
pub(crate) use self::mouse::{Hit, HitMap};
use self::tree::TreeState;
use crate::source::{Action, ArchiveSource};
use crate::theme::{ColorDepth, Theme};
use crate::view::NodeId;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lifecycle {
	Running,
	Quitting,
	Stopping,
}

pub(crate) enum Overlay {
	Help,
	Menu(Menu),
}

enum Outcome {
	Ignored,
	Done,
	Perform(Action),
}

/// The whole TUI minus the terminal: feed it events, let it draw frames.
pub struct App<S> {
	source: S,
	ui: Ui,
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
	pub mouse_capture: bool,
	pub lifecycle: Lifecycle,
	pub hits: HitMap,
	pub issues_area: Option<Rect>,
	pub spinner: ThrobberState,
	pub spinner_drawn: bool,
	pub duplicate_cursor: usize,
	last_click: Option<Click>,
}

impl<S: ArchiveSource> App<S> {
	pub fn new(source: S, depth: ColorDepth) -> Self {
		let library = &source.view().library;
		let jobs = &source.view().workers.jobs;

		let mut expanded = vec![library.root()];
		for job in jobs {
			let mut parent = library.node(job.file).parent;
			while let Some(folder) = parent {
				expanded.push(folder);
				parent = library.node(folder).parent;
			}
		}

		let selected = jobs.first().map_or(library.root(), |job| job.file);
		let mut tree = TreeState::new(expanded, Some(selected));
		tree.sync(library, "");

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
			mouse_capture: true,
			lifecycle: Lifecycle::Running,
			hits: HitMap::default(),
			issues_area: None,
			spinner: ThrobberState::default(),
			spinner_drawn: false,
			duplicate_cursor: 0,
			last_click: None,
		};
		Self { source, ui }
	}

	pub fn source(&self) -> &S {
		&self.source
	}

	pub fn source_mut(&mut self) -> &mut S {
		&mut self.source
	}

	pub fn refresh(&mut self) -> bool {
		self.source.refresh()
	}

	pub fn advance_spinner(&mut self) {
		self.ui.spinner.calc_next();
	}

	pub fn handle_event(&mut self, event: &Event, now: Instant) {
		self.ui.tree.sync(&self.source.view().library, &self.ui.filter);

		let outcome = match event {
			Event::Key(key) if key.kind == KeyEventKind::Press => self.ui.on_key(&self.source, *key),
			Event::Mouse(mouse) => self.ui.on_mouse(&self.source, *mouse, now),
			Event::Paste(text) => self.ui.on_paste(&self.source, text),
			_ => Outcome::Ignored,
		};

		if let Outcome::Perform(action) = outcome {
			self.source.perform(action);
		}

		self.ui.tree.sync(&self.source.view().library, &self.ui.filter);
	}

	pub fn draw(&mut self, frame: &mut Frame) {
		let view = self.source.view();
		self.ui.tree.sync(&view.library, &self.ui.filter);

		let details = match (self.ui.mode, self.ui.tree.selected()) {
			(ViewMode::Details, Some(selected)) => self.source.details(selected),
			_ => None,
		};
		crate::ui::draw(frame, view, details.as_ref(), &mut self.ui);
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
		})
	}

	pub fn menu_items(&self) -> Vec<&'static str> {
		match &self.ui.overlay {
			Some(Overlay::Menu(menu)) => menu.items.iter().map(|item| item.label()).collect(),
			_ => Vec::new(),
		}
	}

	pub fn searching(&self) -> bool {
		self.ui.search.is_some()
	}

	pub fn filter(&self) -> &str {
		&self.ui.filter
	}

	pub fn selected(&self) -> Option<NodeId> {
		self.ui.tree.selected()
	}

	pub fn visible_rows(&self) -> usize {
		self.ui.tree.rows().len()
	}

	pub fn selected_issue(&self) -> usize {
		self.ui.issue_selected
	}
}

impl Ui {
	fn menu_target(&self, source: &dyn ArchiveSource) -> Option<NodeId> {
		match self.mode {
			ViewMode::Issues => source.view().issues.get(self.issue_selected).map(|issue| issue.file),
			ViewMode::Library | ViewMode::Details => self.tree.selected(),
		}
	}

	fn open_menu(&mut self, source: &dyn ArchiveSource, anchor: Option<(u16, u16)>) -> Outcome {
		let Some(target) = self.menu_target(source) else {
			return Outcome::Ignored;
		};

		self.overlay = Some(Overlay::Menu(Menu::for_node(source, target, anchor)));
		Outcome::Done
	}

	fn reveal(&mut self, source: &dyn ArchiveSource, id: NodeId) {
		let library = &source.view().library;
		let hidden = !self.filter.is_empty() && !self.tree.rows().iter().any(|row| row.id == id);
		if hidden {
			self.filter.clear();
		}

		self.tree.reveal(library, id, &self.filter);
	}

	fn open_details(&mut self, source: &dyn ArchiveSource, id: NodeId) {
		self.reveal(source, id);
		self.mode = ViewMode::Details;
	}

	fn escape(&mut self) -> Outcome {
		if self.overlay.take().is_some() {
			return Outcome::Done;
		}

		if self.mode != ViewMode::Library {
			self.mode = ViewMode::Library;
		} else if !self.filter.is_empty() {
			self.filter.clear();
		} else if self.layout.maximized {
			self.layout.maximized = false;
		} else {
			return Outcome::Ignored;
		}
		Outcome::Done
	}
}
