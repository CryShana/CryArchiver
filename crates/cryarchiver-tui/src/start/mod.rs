//! The start menu, shown when the TUI starts without a project: the recent projects and the new project form.

pub mod form;
pub mod validate;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use cryarchiver_engine::text::display_path;
use cryarchiver_project::settings::Speed;
use cryarchiver_project::store::StoreError;
use cryarchiver_project::{ProjectError, Summary};
use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Position;
use tui_input::InputRequest;
use tui_input::backend::crossterm::EventHandler;

use self::form::{FormField, NewProjectForm};
use crate::app::{DOUBLE_CLICK, HitMap, burst_flags};
use crate::recent::{Recent, RecentStore, now_unix_ms};
use crate::theme::{ColorDepth, Theme};
use crate::view::{NoticeLevel, Notification};

const NOTICE_LIFETIME: Duration = Duration::from_secs(8);
const PAGE: usize = 5;
/// How long an open waits for the menu's own read of that same project to end.
const OPEN_WAIT: Duration = Duration::from_secs(2);

pub struct RecentEntry {
	pub recent: Recent,
	/// The project file's name without `.cryarchive`.
	pub name: String,
	pub state: EntryState,
}

/// What reading a recent project found. The list shows at once, and each entry is read in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryState {
	Checking,
	Ready(Summary),
	/// The file is gone from a folder that is still there; "clear missing" removes it.
	Missing,
	/// Its drive, share or folder can't be reached now; it stays in the list.
	Unavailable,
	/// Why the project can't be read.
	Unreadable(String),
}

impl RecentEntry {
	pub fn missing(&self) -> bool {
		self.state == EntryState::Missing
	}

	pub fn summary(&self) -> Option<&Summary> {
		match &self.state {
			EntryState::Ready(summary) => Some(summary),
			_ => None,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartHit {
	Footer(KeyCode),
	Entry(usize),
	NewProject,
	Field(FormField),
	Speed(Speed),
	Modal,
}

/// A question or a message over the menu, answered before anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
	/// Why opening or creating a project failed, in full.
	Error { title: String, text: String },
	/// Esc in a form that was edited.
	Discard,
}

/// The worker that reads the recent projects, one after another.
struct Checks {
	results: Receiver<(u64, usize, EntryState)>,
	worker: Arc<Mutex<Worker>>,
}

/// Shared with the worker thread. The worker looks at `stop` and sets `reading` under the same lock, so once `stop`
/// is set, `reading` names the only project it may still be reading.
#[derive(Default)]
struct Worker {
	stop: bool,
	reading: Option<PathBuf>,
}

impl Checks {
	fn halt(self) -> Arc<Mutex<Worker>> {
		lock(&self.worker).stop = true;
		self.worker
	}
}

fn lock(worker: &Mutex<Worker>) -> MutexGuard<'_, Worker> {
	worker.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct StartMenu {
	store: RecentStore,
	pub(crate) entries: Vec<RecentEntry>,
	pub(crate) selected: usize,
	pub(crate) form: Option<NewProjectForm>,
	pub(crate) dialog: Option<Dialog>,
	notice: Option<(Notification, Instant)>,
	request: Option<PathBuf>,
	pub(crate) opening: Option<String>,
	quit: bool,
	mouse_capture: bool,
	pub(crate) theme: Theme,
	pub(crate) hits: HitMap<StartHit>,
	last_click: Option<(Position, Instant)>,
	last_key: Option<Instant>,
	generation: u64,
	checks: Option<Checks>,
	/// Workers told to stop that may still be finishing their current project.
	halted: Vec<Arc<Mutex<Worker>>>,
}

impl StartMenu {
	/// Without any recent project, the menu starts in the new project form.
	pub fn new(store: RecentStore, depth: ColorDepth) -> Self {
		let loaded = store.load();
		let mut menu = Self {
			store,
			entries: Vec::new(),
			selected: 0,
			form: None,
			dialog: None,
			notice: None,
			request: None,
			opening: None,
			quit: false,
			mouse_capture: true,
			theme: Theme::dark(depth),
			hits: HitMap::default(),
			last_click: None,
			last_key: None,
			generation: 0,
			checks: None,
			halted: Vec::new(),
		};

		menu.set_entries(loaded.entries);
		if menu.entries.is_empty() {
			menu.form = Some(NewProjectForm::new());
		}

		if let Some(text) = loaded.notice {
			menu.notify(NoticeLevel::Warning, text);
		}
		menu
	}

	pub fn entries(&self) -> &[RecentEntry] {
		&self.entries
	}

	pub fn selected(&self) -> usize {
		self.selected
	}

	pub fn form(&self) -> Option<&NewProjectForm> {
		self.form.as_ref()
	}

	pub fn dialog(&self) -> Option<&Dialog> {
		self.dialog.as_ref()
	}

	pub fn notice(&self) -> Option<&Notification> {
		self.notice.as_ref().map(|(notice, _)| notice)
	}

	/// The project the user chose to open, once. The caller opens it and reports back with [`Self::opened`] or
	/// [`Self::open_failed`].
	pub fn take_request(&mut self) -> Option<PathBuf> {
		self.request.take()
	}

	pub fn opened(&mut self, project: &Path) {
		let _ = self.store.record(project, now_unix_ms());
	}

	pub fn open_failed(&mut self, message: String) {
		let name = self.opening.take().unwrap_or_default();
		self.reload();
		self.dialog = Some(Dialog::Error {
			title: format!("{name} didn't open"),
			text: message,
		});
	}

	pub fn should_quit(&self) -> bool {
		self.quit
	}

	pub fn mouse_capture(&self) -> bool {
		self.mouse_capture
	}

	pub fn set_mouse_capture(&mut self, captured: bool) {
		self.mouse_capture = captured;
	}

	/// Takes in the entries read since the last tick and drops an expired notice; returns whether a redraw is due.
	pub fn tick(&mut self) -> bool {
		let mut changed = false;
		while let Some(checks) = &self.checks {
			match checks.results.try_recv() {
				Ok(result) => changed |= self.apply_peek(result),
				Err(mpsc::TryRecvError::Empty) => break,
				Err(mpsc::TryRecvError::Disconnected) => self.checks = None,
			}
		}

		let expired = self
			.notice
			.as_ref()
			.is_some_and(|(_, at)| at.elapsed() >= NOTICE_LIFETIME);
		if expired {
			self.notice = None;
		}
		changed || expired
	}

	/// Waits until every entry has been read, or `timeout` passes; returns whether all were read.
	pub fn wait_for_checks(&mut self, timeout: Duration) -> bool {
		let deadline = Instant::now() + timeout;
		while let Some(checks) = &self.checks {
			match checks
				.results
				.recv_timeout(deadline.saturating_duration_since(Instant::now()))
			{
				Ok(result) => {
					self.apply_peek(result);
				}
				Err(RecvTimeoutError::Disconnected) => self.checks = None,
				Err(RecvTimeoutError::Timeout) => return false,
			}
		}
		true
	}

	/// Stops reading the recent projects before `opening` opens, and keeps them stopped for the session. A worker still
	/// reading another project is left to finish on its own; one reading `opening` itself is waited for, briefly, so
	/// the open doesn't find the project held by this menu.
	pub fn stop_checks(&mut self, opening: &Path) {
		if let Some(checks) = self.checks.take() {
			self.halted.push(checks.halt());
		}

		let deadline = Instant::now() + OPEN_WAIT;
		for worker in self.halted.drain(..) {
			while lock(&worker).reading.as_deref() == Some(opening) && Instant::now() < deadline {
				std::thread::sleep(Duration::from_millis(5));
			}
		}
	}

	/// Whether the recent projects are still being read for the menu.
	pub fn checks_running(&self) -> bool {
		self.checks.is_some()
	}

	/// Whether a worker of this menu, stopped or not, is reading `project` right now.
	pub fn reading(&self, project: &Path) -> bool {
		self.checks
			.iter()
			.map(|checks| &checks.worker)
			.chain(&self.halted)
			.any(|worker| lock(worker).reading.as_deref() == Some(project))
	}

	fn apply_peek(&mut self, (generation, index, state): (u64, usize, EntryState)) -> bool {
		if generation != self.generation {
			return false;
		}

		match self.entries.get_mut(index) {
			Some(entry) => {
				entry.state = state;
				true
			}
			None => false,
		}
	}

	pub fn draw(&mut self, frame: &mut Frame) {
		crate::ui::start::draw(frame, self);
	}

	pub fn handle_event(&mut self, event: &Event, now: Instant) {
		self.dispatch(event, now, false);
	}

	/// Key presses closer together than a person types are a paste and only ever reach text fields.
	pub fn handle_batch(&mut self, events: &[(Event, Instant)]) {
		let key_times: Vec<Instant> = events
			.iter()
			.filter(|(event, _)| is_press(event))
			.map(|&(_, at)| at)
			.collect();
		let mut bursts = burst_flags(&key_times, self.last_key).into_iter();
		if let Some(&last) = key_times.last() {
			self.last_key = Some(last);
		}

		for (event, at) in events {
			let burst = is_press(event) && bursts.next().unwrap_or(false);
			self.dispatch(event, *at, burst);
		}
	}

	fn dispatch(&mut self, event: &Event, now: Instant, burst: bool) {
		if self.opening.is_some() {
			return;
		}

		match event {
			Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(*key, burst),
			Event::Mouse(mouse) => {
				let position = Position::new(mouse.column, mouse.row);
				let scrolls = self.form.is_none() && self.dialog.is_none();
				match mouse.kind {
					MouseEventKind::Down(MouseButton::Left) => {
						let double = self.register_click(position, now);
						self.on_click(position, double);
					}
					MouseEventKind::ScrollDown if scrolls => self.move_selection(1),
					MouseEventKind::ScrollUp if scrolls => self.move_selection(-1),
					_ => {}
				}
			}
			Event::Paste(text) if self.dialog.is_none() => self.on_paste(text),
			_ => {}
		}
	}

	fn typing(&self) -> bool {
		self.dialog.is_none()
			&& self
				.form
				.as_ref()
				.is_some_and(|form| form.confirming().is_some() || form.focus().is_text())
	}

	fn on_key(&mut self, key: KeyEvent, burst: bool) {
		if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
			self.quit = true;
			return;
		}

		if burst && (!self.typing() || key.code == KeyCode::Enter) {
			return;
		}

		if self.dialog.is_some() {
			self.on_dialog_key(key.code);
			return;
		}

		if key.code == KeyCode::F(2) {
			self.mouse_capture = !self.mouse_capture;
			return;
		}

		if self.form.is_some() {
			self.on_form_key(key);
		} else {
			self.on_recents_key(key);
		}
	}

	fn on_dialog_key(&mut self, code: KeyCode) {
		match (&self.dialog, code) {
			(Some(Dialog::Discard), KeyCode::Enter) => {
				self.dialog = None;
				self.form = None;
			}
			(Some(_), KeyCode::Enter | KeyCode::Esc) => self.dialog = None,
			_ => {}
		}
	}

	fn on_recents_key(&mut self, key: KeyEvent) {
		match key.code {
			KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
			KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
			KeyCode::PageUp => self.move_selection(-PAGE.cast_signed()),
			KeyCode::PageDown => self.move_selection(PAGE.cast_signed()),
			KeyCode::Home => self.selected = 0,
			KeyCode::End => self.selected = self.entries.len().saturating_sub(1),
			KeyCode::Enter => self.open_selected(),
			KeyCode::Char('n') => self.form = Some(NewProjectForm::new()),
			KeyCode::Char('x') | KeyCode::Delete => self.remove_selected(),
			KeyCode::Char('c') => self.clear_missing(),
			KeyCode::Char('q') => self.quit = true,
			_ => {}
		}
	}

	fn on_form_key(&mut self, key: KeyEvent) {
		let Some(form) = self.form.as_mut() else { return };

		if let Some(input) = form.confirming_mut() {
			match key.code {
				KeyCode::Esc => form.cancel_delete(),
				KeyCode::Enter => {
					if !form.confirm_delete() {
						self.notify(
							NoticeLevel::Warning,
							"deleting originals stays off: type DELETE exactly".into(),
						);
					}
				}
				_ => {
					input.handle_event(&Event::Key(key));
				}
			}
			return;
		}

		let focus = form.focus();
		match key.code {
			KeyCode::Esc => self.leave_form(),
			KeyCode::Tab | KeyCode::Down => form.move_focus(1),
			KeyCode::BackTab | KeyCode::Up => form.move_focus(-1),
			KeyCode::Left if focus == FormField::Speed => form.set_speed(next_speed(form.speed(), -1)),
			KeyCode::Right if focus == FormField::Speed => form.set_speed(next_speed(form.speed(), 1)),
			KeyCode::Enter if focus.is_text() || focus == FormField::Speed => form.move_focus(1),
			KeyCode::Enter | KeyCode::Char(' ') if !focus.is_text() => self.activate(focus),
			KeyCode::Char('q') if !focus.is_text() => self.quit = true,
			_ if focus.is_text() => form.edit(focus, |input| {
				input.handle_event(&Event::Key(key));
			}),
			_ => {}
		}
	}

	/// An edited form asks before the typed values are thrown away.
	fn leave_form(&mut self) {
		if self.form.as_ref().is_some_and(NewProjectForm::edited) {
			self.dialog = Some(Dialog::Discard);
		} else {
			self.form = None;
		}
	}

	fn activate(&mut self, field: FormField) {
		let Some(form) = self.form.as_mut() else { return };

		match field {
			FormField::Speed => form.set_speed(next_speed(form.speed(), 1)),
			FormField::DeleteOriginals => form.toggle_delete(),
			FormField::Create => self.create(),
			FormField::Cancel => self.leave_form(),
			FormField::Source | FormField::Destination | FormField::Name => {}
		}
	}

	/// A new project joins the list at once, so it stays reachable even when opening it fails.
	fn create(&mut self) {
		let Some(form) = self.form.as_mut() else { return };

		let Some(path) = form.create() else {
			if let Some(error) = form.error() {
				self.dialog = Some(Dialog::Error {
					title: "The project wasn't created".into(),
					text: error.to_owned(),
				});
			}
			return;
		};
		let warning = form.take_warning();

		match self.store.record(&path, now_unix_ms()) {
			Ok(recents) => self.set_entries(recents),
			Err(err) => self.notify(NoticeLevel::Warning, not_saved(&err)),
		}
		if let Some(warning) = warning {
			self.notify(NoticeLevel::Warning, warning);
		}

		self.selected = 0;
		self.form = None;
		self.request_open(path);
	}

	fn on_paste(&mut self, text: &str) {
		let Some(form) = self.form.as_mut() else { return };

		let text = pasted(text);
		let insert = |input: &mut tui_input::Input| {
			for c in text.chars() {
				input.handle(InputRequest::InsertChar(c));
			}
		};

		if let Some(input) = form.confirming_mut() {
			insert(input);
		} else if form.focus().is_text() {
			form.edit(form.focus(), insert);
		}
	}

	fn register_click(&mut self, position: Position, now: Instant) -> bool {
		let double = self
			.last_click
			.is_some_and(|(last, at)| last == position && now.duration_since(at) <= DOUBLE_CLICK);
		self.last_click = if double { None } else { Some((position, now)) };
		double
	}

	/// While a dialog or the `DELETE` prompt is open, only its footer chips take clicks.
	fn on_click(&mut self, position: Position, double: bool) {
		let hit = self.hits.at(position);
		let modal = self.dialog.is_some() || self.form.as_ref().is_some_and(|form| form.confirming().is_some());
		if modal {
			if let Some(StartHit::Footer(code)) = hit {
				self.on_key(KeyEvent::new(code, KeyModifiers::NONE), false);
			}
			return;
		}

		match hit {
			Some(StartHit::Footer(code)) => self.on_key(KeyEvent::new(code, KeyModifiers::NONE), false),
			Some(StartHit::Entry(index)) => {
				self.selected = index;
				if double {
					self.open_selected();
				}
			}
			Some(StartHit::NewProject) => self.form = Some(NewProjectForm::new()),
			Some(StartHit::Field(field)) => {
				if let Some(form) = self.form.as_mut() {
					form.set_focus(field);
				}

				if !field.is_text() && field != FormField::Speed {
					self.activate(field);
				}
			}
			Some(StartHit::Speed(speed)) => {
				if let Some(form) = self.form.as_mut() {
					form.set_focus(FormField::Speed);
					form.set_speed(speed);
				}
			}
			Some(StartHit::Modal) | None => {}
		}
	}

	fn move_selection(&mut self, step: isize) {
		let last = self.entries.len().saturating_sub(1);
		self.selected = self.selected.saturating_add_signed(step).min(last);
	}

	fn open_selected(&mut self) {
		let Some(entry) = self.entries.get(self.selected) else {
			return;
		};

		let refusal = match entry.state {
			EntryState::Missing => Some("the project file is missing; x removes it from the list"),
			EntryState::Unavailable => Some("the project's drive, share or folder can't be reached right now"),
			_ => None,
		};
		if let Some(text) = refusal {
			self.notify(NoticeLevel::Warning, text.into());
			return;
		}

		let path = entry.recent.path.clone();
		self.request_open(path);
	}

	/// The worker is told to stop at once; [`Self::stop_checks`] runs right before the open.
	fn request_open(&mut self, path: PathBuf) {
		if let Some(checks) = self.checks.take() {
			self.halted.push(checks.halt());
		}

		self.opening = Some(project_name(&path));
		self.request = Some(path);
	}

	fn remove_selected(&mut self) {
		let Some(entry) = self.entries.get(self.selected) else {
			return;
		};

		let path = entry.recent.path.clone();
		match self.store.remove(&path) {
			Ok(recents) => self.set_entries(recents),
			Err(err) => self.notify(NoticeLevel::Error, not_saved(&err)),
		}
	}

	fn clear_missing(&mut self) {
		let before = self.entries.len();
		match self.store.clear_missing() {
			Ok(recents) => {
				self.set_entries(recents);
				let removed = before.saturating_sub(self.entries.len());
				let text = match removed {
					0 => "no project in the list is missing".to_owned(),
					1 => "removed 1 missing project from the list".to_owned(),
					count => format!("removed {count} missing projects from the list"),
				};
				self.notify(NoticeLevel::Info, text);
			}
			Err(err) => self.notify(NoticeLevel::Error, not_saved(&err)),
		}
	}

	fn reload(&mut self) {
		let loaded = self.store.load();
		self.set_entries(loaded.entries);
	}

	/// Shows the entries at once as being checked, and reads them on a worker thread.
	fn set_entries(&mut self, recents: Vec<Recent>) {
		let paths: Vec<PathBuf> = recents.iter().map(|recent| recent.path.clone()).collect();
		self.entries = recents
			.into_iter()
			.map(|recent| RecentEntry {
				name: project_name(&recent.path),
				state: EntryState::Checking,
				recent,
			})
			.collect();
		self.selected = self.selected.min(self.entries.len().saturating_sub(1));

		if let Some(checks) = self.checks.take() {
			self.halted.push(checks.halt());
		}
		self.halted.retain(|worker| lock(worker).reading.is_some());

		self.generation += 1;
		self.checks = (!paths.is_empty()).then(|| spawn_checks(self.generation, paths));
	}

	fn notify(&mut self, level: NoticeLevel, text: String) {
		self.notice = Some((Notification { level, text }, Instant::now()));
	}
}

/// The thread is never joined: a stopped worker finishes the project it is on and returns.
fn spawn_checks(generation: u64, paths: Vec<PathBuf>) -> Checks {
	let (sender, results) = mpsc::channel();
	let worker = Arc::new(Mutex::new(Worker::default()));
	let shared = worker.clone();
	std::thread::spawn(move || {
		for (index, path) in paths.into_iter().enumerate() {
			{
				let mut state = lock(&shared);
				if state.stop {
					return;
				}
				state.reading = Some(path.clone());
			}

			let state = peek(&path);
			lock(&shared).reading = None;
			if sender.send((generation, index, state)).is_err() {
				return;
			}
		}
	});

	Checks { results, worker }
}

fn peek(path: &Path) -> EntryState {
	match cryarchiver_project::peek(path) {
		Ok(summary) => EntryState::Ready(summary),
		Err(ProjectError::Store(StoreError::Missing)) => EntryState::Missing,
		Err(ProjectError::Store(StoreError::Unavailable)) => EntryState::Unavailable,
		Err(err) => EntryState::Unreadable(err.to_string()),
	}
}

fn project_name(path: &Path) -> String {
	path.file_stem()
		.map_or_else(|| display_path(path), |stem| stem.to_string_lossy().into_owned())
}

fn not_saved(err: &io::Error) -> String {
	format!("the recent projects couldn't be saved: {err}")
}

fn next_speed(speed: Speed, step: isize) -> Speed {
	const SPEEDS: [Speed; 3] = [Speed::Slow, Speed::Moderate, Speed::Fast];

	let index = SPEEDS.iter().position(|&candidate| candidate == speed).unwrap_or(1);
	SPEEDS[(index + SPEEDS.len()).saturating_add_signed(step) % SPEEDS.len()]
}

/// Drops control characters, and the quotes a file manager adds around a dropped path.
fn pasted(text: &str) -> String {
	let clean: String = text.chars().filter(|c| !c.is_control()).collect();
	let unquoted = clean
		.trim()
		.strip_prefix('"')
		.and_then(|rest| rest.strip_suffix('"'))
		.filter(|inner| !inner.contains('"'))
		.map(str::to_owned);
	unquoted.unwrap_or(clean)
}

fn is_press(event: &Event) -> bool {
	matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press)
}

/// When the project was last opened, relative to `now`. Dates are UTC, like the log's times.
pub fn opened_ago(now_unix_ms: i64, opened_unix_ms: i64) -> String {
	const MINUTE: i64 = 60;
	const HOUR: i64 = 60 * MINUTE;
	const DAY: i64 = 24 * HOUR;
	const TWO_DAYS: i64 = 2 * DAY;
	const WEEK: i64 = 7 * DAY;

	let seconds = (now_unix_ms - opened_unix_ms).max(0) / 1000;
	match seconds {
		..MINUTE => "just now".to_owned(),
		MINUTE..HOUR => format!("{} min ago", seconds / MINUTE),
		HOUR..DAY => format!("{} h ago", seconds / HOUR),
		DAY..TWO_DAYS => "yesterday".to_owned(),
		TWO_DAYS..WEEK => format!("{} days ago", seconds / DAY),
		_ => {
			let (year, month, day) = civil_date(opened_unix_ms.div_euclid(1000 * DAY));
			format!("on {year}-{month:02}-{day:02}")
		}
	}
}

/// The proleptic Gregorian date of a day count since 1970-01-01.
fn civil_date(days: i64) -> (i64, i64, i64) {
	let shifted = days + 719_468;
	let era = shifted.div_euclid(146_097);
	let day_of_era = shifted.rem_euclid(146_097);
	let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_index = (5 * day_of_year + 2) / 153;
	let day = day_of_year - (153 * month_index + 2) / 5 + 1;
	let month = if month_index < 10 {
		month_index + 3
	} else {
		month_index - 9
	};
	let year = year_of_era + era * 400 + i64::from(month <= 2);
	(year, month, day)
}

#[cfg(test)]
mod tests {
	use super::*;

	const MINUTE_MS: i64 = 60_000;
	const DAY_MS: i64 = 24 * 60 * MINUTE_MS;

	#[test]
	fn open_times_read_relative_then_as_a_date() {
		let now = 1_790_000_000_000;

		assert_eq!(opened_ago(now, now - 5_000), "just now");
		assert_eq!(opened_ago(now, now - 5 * MINUTE_MS), "5 min ago");
		assert_eq!(opened_ago(now, now - 3 * 60 * MINUTE_MS), "3 h ago");
		assert_eq!(opened_ago(now, now - DAY_MS - 1), "yesterday");
		assert_eq!(opened_ago(now, now - 3 * DAY_MS), "3 days ago");
		assert_eq!(opened_ago(now, 0), "on 1970-01-01");
		assert_eq!(opened_ago(now, 1_709_164_800_000), "on 2024-02-29");
		assert_eq!(opened_ago(now, now + MINUTE_MS), "just now");
	}

	#[test]
	fn a_dropped_path_loses_its_quotes() {
		assert_eq!(pasted("\"D:\\My Photos\"\r\n"), "D:\\My Photos");
		assert_eq!(pasted("D:\\Photos\n"), "D:\\Photos");
		assert_eq!(pasted("a \"b\" c"), "a \"b\" c");
	}

	fn halted_worker(reading: &Path) -> Arc<Mutex<Worker>> {
		Arc::new(Mutex::new(Worker {
			stop: true,
			reading: Some(reading.to_path_buf()),
		}))
	}

	#[test]
	fn an_open_waits_only_for_a_read_of_the_same_project() {
		let dir = tempfile::tempdir().unwrap();
		let mut menu = StartMenu::new(RecentStore::at(dir.path().join("recent.bin")), ColorDepth::TrueColor);
		let (opening, other) = (dir.path().join("a.cryarchive"), dir.path().join("b.cryarchive"));

		menu.halted.push(halted_worker(&other));
		let started = Instant::now();
		menu.stop_checks(&opening);
		assert!(started.elapsed() < Duration::from_millis(500));

		let same = halted_worker(&opening);
		menu.halted.push(same.clone());
		let finisher = std::thread::spawn(move || {
			std::thread::sleep(Duration::from_millis(100));
			lock(&same).reading = None;
		});
		let started = Instant::now();
		menu.stop_checks(&opening);
		let waited = started.elapsed();
		finisher.join().unwrap();

		assert!(waited >= Duration::from_millis(50), "{waited:?}");
		assert!(waited < OPEN_WAIT, "{waited:?}");
		assert!(!menu.reading(&opening));
	}

	#[test]
	fn speeds_cycle_both_ways() {
		assert_eq!(next_speed(Speed::Moderate, 1), Speed::Fast);
		assert_eq!(next_speed(Speed::Fast, 1), Speed::Slow);
		assert_eq!(next_speed(Speed::Slow, -1), Speed::Fast);
	}
}
