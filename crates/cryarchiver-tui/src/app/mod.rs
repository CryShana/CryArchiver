mod burst;
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

pub(crate) use self::burst::{BURST_GAP, burst_flags};
pub(crate) use self::menu::Menu;
pub(crate) use self::mouse::{Bar, DOUBLE_CLICK, Hit, HitMap};
use self::mouse::{Click, ScrollDrag};
pub(crate) use self::settings::{FAST_NEVER_MEASURES_VIDEO, QUALITY_FIELDS, QualityField, SETTINGS_ROWS, SettingsRow};
pub use self::tree::Selection;
use self::tree::TreeState;
use crate::source::{Action, ArchiveSource};
use crate::theme::{ColorDepth, Theme};
use crate::view::{FileDetails, FileId, Filter, IssueId, Library, Log, NodeId, NoticeLevel, Notification, StateFilter};

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
	EditQuality,
	ConfirmDelete,
	ConfirmRestart,
	ConfirmQuit,
}

pub(crate) enum Overlay {
	Help,
	Menu(Menu),
	Settings(usize),
	/// A quality row of the settings being typed into; `row` indexes [`SETTINGS_ROWS`].
	EditQuality {
		row: usize,
		input: Input,
	},
	ConfirmDelete(Input),
	ConfirmRestart,
	ConfirmQuit,
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
	issue_log: Option<IssueLogCache>,
	toast: Option<(Notification, Instant)>,
	clipboard: Option<String>,
	last_key: Option<Instant>,
}

/// Where `g` is in an original's list of duplicates. It lasts only while the selection stays where `g` put it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DuplicateCycle {
	pub original: FileId,
	pub next: usize,
	pub landed: FileId,
}

struct IssueLogCache {
	issue: IssueId,
	attempt: u32,
	generation: u64,
	lines: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
	pub maximized: bool,
	pub log_open: bool,
	pub log_maximized: bool,
}

/// How far the Log is scrolled back from its newest line. Away from the newest line it holds on to the number of the
/// line at the top, so lines arriving later don't move what is on screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LogScroll {
	top: Option<u64>,
	page: usize,
}

impl LogScroll {
	/// A page that now reaches the newest line drops the anchor, so the log follows new lines again.
	pub fn set_page(&mut self, rows: usize, log: &Log) {
		self.page = rows;
		if self.newer(log) == 0 {
			self.top = None;
		}
	}

	/// How many lines newer than the top one are scrolled out of view.
	pub fn newer(&self, log: &Log) -> usize {
		let Some(top) = self.top else { return 0 };
		let back = log.pushed().saturating_sub(1).saturating_sub(top);
		usize::try_from(back).unwrap_or(usize::MAX).min(self.deepest(log))
	}

	pub fn scroll_by(&mut self, log: &Log, rows: isize) {
		self.scroll_to(log, self.newer(log).saturating_add_signed(rows));
	}

	/// Shows the page whose top line is `back` lines older than the newest one.
	pub fn scroll_to(&mut self, log: &Log, back: usize) {
		let back = back.min(self.deepest(log));
		let back_lines = u64::try_from(back).unwrap_or(u64::MAX);
		self.top = (back > 0).then(|| log.pushed().saturating_sub(1).saturating_sub(back_lines));
	}

	pub fn page_by(&mut self, log: &Log, pages: isize) {
		let rows = isize::try_from(self.page.max(1)).unwrap_or(isize::MAX);
		self.scroll_by(log, pages.saturating_mul(rows));
	}

	pub fn jump_to_newest(&mut self) {
		self.top = None;
	}

	pub fn jump_to_oldest(&mut self, log: &Log) {
		self.scroll_by(log, isize::MAX);
	}

	/// The farthest back the Log scrolls: its oldest page.
	pub fn deepest(&self, log: &Log) -> usize {
		log.len().saturating_sub(self.page)
	}
}

pub(crate) struct Ui {
	pub theme: Theme,
	pub layout: Layout,
	pub mode: ViewMode,
	pub overlay: Option<Overlay>,
	pub search: Option<Input>,
	pub filter: Filter,
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
	pub duplicate_cycle: Option<DuplicateCycle>,
	pub actions: Vec<Action>,
	pub log_scroll: LogScroll,
	/// A log line the user copied, until the app moves it to the clipboard.
	pub copied: Option<String>,
	/// The overlay the quit question replaced, shown again when the user stays.
	pub before_quit: Option<Overlay>,
	last_click: Option<Click>,
	scroll_drag: Option<ScrollDrag>,
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
		tree.sync(&source, &Filter::default());

		let ui = Ui {
			theme: Theme::dark(depth),
			layout: Layout {
				maximized: false,
				log_open: true,
				log_maximized: false,
			},
			mode: ViewMode::Library,
			overlay: None,
			search: None,
			filter: Filter::default(),
			tree,
			issue_selected: 0,
			attempt_back: 0,
			mouse_capture: true,
			lifecycle: Lifecycle::Running,
			hits: HitMap::default(),
			issues_area: None,
			spinner: ThrobberState::default(),
			spinner_drawn: false,
			duplicate_cycle: None,
			actions: Vec::new(),
			log_scroll: LogScroll::default(),
			copied: None,
			before_quit: None,
			last_click: None,
			scroll_drag: None,
		};
		Self {
			source,
			ui,
			generation: 0,
			details: None,
			issue_log: None,
			toast: None,
			clipboard: None,
			last_key: None,
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
			self.ui.tree.values_changed();
		}
		self.take_notifications() || changed
	}

	pub fn advance_spinner(&mut self) {
		self.ui.spinner.calc_next();
	}

	/// Handles one event on its own, as typed input; the terminal runner feeds batches through [`Self::handle_batch`].
	pub fn handle_event(&mut self, event: &Event, now: Instant) {
		self.dispatch(event, now, false);
	}

	/// Handles events that arrived together, each with the time it was read. Key presses closer together than a
	/// person types are a paste and only ever reach text fields.
	pub fn handle_batch(&mut self, events: &[(Event, Instant)]) {
		let key_times: Vec<Instant> = events
			.iter()
			.filter(|(event, _)| is_press(event))
			.map(|&(_, at)| at)
			.collect();
		let mut bursts = burst::burst_flags(&key_times, self.last_key).into_iter();
		if let Some(&last) = key_times.last() {
			self.last_key = Some(last);
		}

		for (event, at) in events {
			let burst = is_press(event) && bursts.next().unwrap_or(false);
			self.dispatch(event, *at, burst);
		}
	}

	fn dispatch(&mut self, event: &Event, now: Instant, burst: bool) {
		self.ui.tree.sync(&self.source, &self.ui.filter);

		match event {
			Event::Key(key) if key.kind == KeyEventKind::Press => {
				self.ui.on_key(&self.source, *key, burst);
			}
			Event::Mouse(mouse) => {
				self.ui.on_mouse(&self.source, *mouse, now);
			}
			Event::Paste(text) => self.ui.on_paste(&self.source, text),
			_ => {}
		}

		self.perform_pending();
		self.take_copied();
		self.ui.tree.sync(&self.source, &self.ui.filter);

		let target = self.ui.tree.target();
		if self
			.ui
			.duplicate_cycle
			.is_some_and(|cycle| !matches!(target, Some(Selection::File { file, .. }) if file == cycle.landed))
		{
			self.ui.duplicate_cycle = None;
		}
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

	fn take_copied(&mut self) {
		let Some(text) = self.ui.copied.take() else { return };

		let notice = Notification {
			level: NoticeLevel::Info,
			text: format!("copied {text}"),
		};
		self.toast = Some((notice, Instant::now()));
		self.clipboard = Some(text);
	}

	/// Shows the most severe of the new notifications; a fresh toast that is more severe stays up.
	fn take_notifications(&mut self) -> bool {
		let Some(worst) = self
			.source
			.take_notifications()
			.into_iter()
			.max_by_key(|notice| notice.level)
		else {
			return false;
		};

		let current = self.toast.as_ref().filter(|(_, at)| at.elapsed() < TOAST_LIFETIME);
		if current.is_some_and(|(shown, _)| shown.level > worst.level) {
			return false;
		}

		self.toast = Some((worst, Instant::now()));
		true
	}

	pub fn draw(&mut self, frame: &mut Frame) {
		self.ui.tree.sync(&self.source, &self.ui.filter);
		self.toast = self.toast.take().filter(|(_, at)| at.elapsed() < TOAST_LIFETIME);

		self.refresh_details();
		self.refresh_issue_log();
		let fetched = crate::ui::Fetched {
			details: self.details.as_ref().and_then(|(_, _, details)| details.as_ref()),
			issue_log: self.issue_log.as_ref().map(|cache| crate::ui::AttemptLog {
				attempt: cache.attempt,
				lines: cache.lines.as_deref(),
			}),
			toast: self.toast.as_ref().map(|(notification, _)| notification),
		};
		crate::ui::draw(frame, &self.source, &fetched, &mut self.ui);
	}

	/// Fetches only the attempt on screen, again only when it or the source changed.
	fn refresh_issue_log(&mut self) {
		let issues = &self.source.view().issues;
		let selected = issues.get(self.ui.issue_selected.min(issues.len().saturating_sub(1)));
		let wanted = selected
			.filter(|issue| self.ui.mode == ViewMode::Issues && issue.attempts > 0)
			.map(|issue| {
				let back = u32::try_from(self.ui.attempt_back).unwrap_or(u32::MAX);
				(issue.id, issue.attempts.saturating_sub(back).max(1))
			});

		let Some((issue, attempt)) = wanted else {
			self.issue_log = None;
			return;
		};

		let fresh = self.issue_log.as_ref().is_some_and(|cache| {
			cache.issue == issue && cache.attempt == attempt && cache.generation == self.generation
		});
		if !fresh {
			self.issue_log = Some(IssueLogCache {
				issue,
				attempt,
				generation: self.generation,
				lines: self.source.issue_log(issue, attempt),
			});
		}
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
		self.ui.search = None;
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

	pub fn log_maximized(&self) -> bool {
		self.ui.layout.log_maximized
	}

	pub fn mode(&self) -> ViewMode {
		self.ui.mode
	}

	pub fn overlay(&self) -> Option<OverlayKind> {
		self.ui.overlay.as_ref().map(|overlay| match overlay {
			Overlay::Help => OverlayKind::Help,
			Overlay::Menu(_) => OverlayKind::Menu,
			Overlay::Settings(_) => OverlayKind::Settings,
			Overlay::EditQuality { .. } => OverlayKind::EditQuality,
			Overlay::ConfirmDelete(_) => OverlayKind::ConfirmDelete,
			Overlay::ConfirmRestart => OverlayKind::ConfirmRestart,
			Overlay::ConfirmQuit => OverlayKind::ConfirmQuit,
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

	/// The name query the Library is filtered by.
	pub fn filter(&self) -> &str {
		&self.ui.filter.query
	}

	pub fn state_filter(&self) -> Option<StateFilter> {
		self.ui.filter.state
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

	/// The index of the Library row at the top of the view.
	pub fn library_scroll(&self) -> usize {
		self.ui.tree.scroll()
	}

	/// How many lines newer than the Log's top line are scrolled out of view.
	pub fn log_scrolled_back(&self) -> usize {
		self.ui.log_scroll.newer(&self.source.view().log)
	}

	pub fn selected_issue(&self) -> usize {
		self.ui.issue_selected
	}

	pub fn take_clipboard(&mut self) -> Option<String> {
		self.clipboard.take().or_else(|| self.source.take_clipboard())
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

		self.overlay = Some(Overlay::Menu(Menu::new(source, self, target, anchor)));
		Outcome::Done
	}

	/// Clearing the filter keeps what was selected in it, now shown in its own folder.
	fn set_filter(&mut self, library: &Library, filter: Filter) {
		if filter == self.filter {
			return;
		}

		let cleared = filter.is_empty();
		self.filter.clone_from(&filter);
		self.actions.push(Action::SetFilter(filter));

		match self.tree.selected() {
			Some(Selection::File { file, folder }) if cleared => self.tree.reveal_file(library, folder, file),
			Some(Selection::Folder(folder)) if cleared => self.tree.reveal_folder(library, folder),
			_ => {}
		}
	}

	fn set_query(&mut self, library: &Library, query: String) {
		let state = self.filter.state;
		self.set_filter(library, Filter { query, state });
	}

	/// The state already on is turned off again; the name query stays either way.
	fn toggle_state_filter(&mut self, library: &Library, state: StateFilter) {
		let filter = Filter {
			query: self.filter.query.clone(),
			state: (self.filter.state != Some(state)).then_some(state),
		};
		self.set_filter(library, filter);
	}

	fn reveal_file(&mut self, source: &dyn ArchiveSource, folder: NodeId, file: FileId) {
		let visible = self
			.tree
			.rows()
			.iter()
			.any(|row| matches!(row.kind, tree::RowKind::File { file: id, .. } if id == file));
		let library = &source.view().library;
		if !self.filter.is_empty() && !visible {
			self.set_filter(library, Filter::default());
		}

		self.tree.reveal_file(library, folder, file);
	}

	fn open_details(&mut self, source: &dyn ArchiveSource, folder: NodeId, file: FileId) {
		self.reveal_file(source, folder, file);
		self.mode = ViewMode::Details;
	}

	fn escape(&mut self, source: &dyn ArchiveSource) -> Outcome {
		if self.overlay.take().is_some() {
			return Outcome::Done;
		}

		if self.layout.log_maximized {
			self.layout.log_maximized = false;
			return Outcome::Done;
		}

		let library = &source.view().library;
		if self.mode != ViewMode::Library {
			self.mode = ViewMode::Library;
		} else if self.filter.state.is_some() {
			self.set_filter(library, Filter::name(self.filter.query.clone()));
		} else if !self.filter.is_empty() {
			self.set_filter(library, Filter::default());
		} else if self.layout.maximized {
			self.layout.maximized = false;
		} else {
			return Outcome::Ignored;
		}
		Outcome::Done
	}

	fn toggle_maximized(&mut self) {
		self.layout.maximized = !self.layout.maximized;
		self.layout.log_maximized = false;
	}

	fn toggle_log_maximized(&mut self) {
		self.layout.log_maximized = !self.layout.log_maximized;
		self.layout.maximized = false;
	}

	fn select_issue(&mut self, index: usize) {
		if index != self.issue_selected {
			self.attempt_back = 0;
		}
		self.issue_selected = index;
	}
}

fn is_press(event: &Event) -> bool {
	matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::view::{ClockTime, LogLine, LogMarker};

	fn log_of(capacity: usize, lines: std::ops::Range<u32>) -> Log {
		let mut log = Log::with_capacity(capacity);
		for n in lines {
			push(&mut log, n);
		}
		log
	}

	fn push(log: &mut Log, n: u32) {
		log.push(LogLine {
			time: ClockTime(n),
			marker: LogMarker::Action,
			message: n.to_string(),
		});
	}

	fn top(scroll: LogScroll, log: &Log) -> String {
		log.newest_first().nth(scroll.newer(log)).unwrap().message.clone()
	}

	#[test]
	fn a_scrolled_log_keeps_its_top_line_while_the_oldest_ones_fall_off() {
		let mut log = log_of(10, 0..10);
		let mut scroll = LogScroll::default();
		scroll.set_page(3, &log);
		scroll.scroll_by(&log, 4);
		assert_eq!(top(scroll, &log), "5");

		push(&mut log, 10);
		push(&mut log, 11);
		assert_eq!(scroll.newer(&log), 6);
		assert_eq!(top(scroll, &log), "5");

		for n in 12..20 {
			push(&mut log, n);
		}
		assert_eq!(
			top(scroll, &log),
			"12",
			"a line that fell off leaves the oldest page on screen"
		);
	}

	#[test]
	fn scrolling_stops_at_the_oldest_page_and_back_at_the_newest_follows_again() {
		let mut log = log_of(50, 0..10);
		let mut scroll = LogScroll::default();
		scroll.set_page(4, &log);

		scroll.page_by(&log, 5);
		assert_eq!(scroll.newer(&log), 6);
		scroll.jump_to_oldest(&log);
		assert_eq!(scroll.newer(&log), 6);

		scroll.scroll_by(&log, -6);
		push(&mut log, 10);
		assert_eq!(scroll.newer(&log), 0, "at the newest line the log follows new lines");
	}

	#[test]
	fn a_taller_page_that_shows_the_newest_line_follows_new_lines_again() {
		let mut log = log_of(50, 0..10);
		let mut scroll = LogScroll::default();
		scroll.set_page(3, &log);
		scroll.scroll_by(&log, 2);

		scroll.set_page(20, &log);
		for n in 10..25 {
			push(&mut log, n);
		}
		scroll.set_page(20, &log);

		assert_eq!(scroll.newer(&log), 0);
	}
}
