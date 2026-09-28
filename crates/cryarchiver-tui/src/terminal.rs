use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
	self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
};
use ratatui::crossterm::execute;

use crate::app::{App, BURST_GAP};
use crate::recent::RecentStore;
use crate::source::ArchiveSource;
use crate::start::StartMenu;
use crate::theme::ColorDepth;

const FRAME: Duration = Duration::from_millis(33);
const SPINNER_STEP: Duration = Duration::from_millis(100);
const INPUT_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum RunError {
	#[error("the TUI needs an interactive terminal; run it from a terminal window, or pass files to use the CLI")]
	NotATerminal,
	#[error("the terminal stopped delivering input")]
	InputClosed,
	#[error("terminal error: {0}")]
	Io(#[from] io::Error),
}

type Events = Receiver<io::Result<(Event, Instant)>>;

/// Runs the TUI until the user quits. The source is shut down while a "stopping" frame is on screen and is dropped
/// only after the terminal has been restored, because stopping a real run can take seconds.
pub fn run<S: ArchiveSource>(source: S) -> Result<(), RunError> {
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Err(RunError::NotATerminal);
	}

	let mut app = App::new(source, ColorDepth::detect());
	let mut terminal = match start_terminal() {
		Ok(terminal) => terminal,
		Err(err) => {
			app.shutdown();
			return Err(err);
		}
	};
	app.set_mouse_capture(capture_mouse());

	let input = Input::spawn();
	let result = pump(&mut terminal, &mut app, &input.events);
	input.stop();

	finish(&mut terminal, app, result)
}

/// Runs the start menu until the user quits or a project opens, then that project until the user quits. `open`
/// runs while the menu shows that the project is opening; its error goes back to the menu.
pub fn run_start_menu<S, F>(store: RecentStore, mut open: F) -> Result<(), RunError>
where
	S: ArchiveSource,
	F: FnMut(&Path) -> Result<S, String>,
{
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Err(RunError::NotATerminal);
	}

	let depth = ColorDepth::detect();
	let mut menu = StartMenu::new(store, depth);
	let mut terminal = start_terminal()?;
	menu.set_mouse_capture(capture_mouse());

	let input = Input::spawn();
	let source = match pump_menu(&mut terminal, &mut menu, &input.events, &mut open) {
		Ok(Some(source)) => source,
		ended => {
			input.stop();
			release_input_modes();
			ratatui::restore();
			return ended.map(|_| ());
		}
	};

	// Keys pressed while the project was opening were meant for the menu, not for the dashboard.
	while input.events.try_recv().is_ok() {}

	let mut app = App::new(source, depth);
	app.set_mouse_capture(menu.mouse_capture());
	let result = pump(&mut terminal, &mut app, &input.events);
	input.stop();

	finish(&mut terminal, app, result)
}

fn start_terminal() -> Result<DefaultTerminal, RunError> {
	let terminal = ratatui::try_init().inspect_err(|_| ratatui::restore())?;

	release_input_modes_on_panic();
	let _ = execute!(io::stdout(), EnableBracketedPaste);
	Ok(terminal)
}

fn capture_mouse() -> bool {
	execute!(io::stdout(), EnableMouseCapture).is_ok()
}

fn finish<S: ArchiveSource>(
	terminal: &mut DefaultTerminal,
	mut app: App<S>,
	result: Result<(), RunError>,
) -> Result<(), RunError> {
	app.begin_shutdown();
	let _ = terminal.draw(|frame| app.draw(frame));
	app.shutdown();

	release_input_modes();
	ratatui::restore();
	drop(app);
	result
}

fn release_input_modes() {
	let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
}

fn release_input_modes_on_panic() {
	let restore_terminal = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		release_input_modes();
		restore_terminal(info);
	}));
}

/// Crossterm's blocking read runs on its own thread, so the UI thread only ever waits on the channel.
struct Input {
	events: Events,
	stop: Arc<AtomicBool>,
	reader: JoinHandle<()>,
}

impl Input {
	fn spawn() -> Self {
		let stop = Arc::new(AtomicBool::new(false));
		let (events, reader) = spawn_input(stop.clone());
		Self { events, stop, reader }
	}

	fn stop(self) {
		self.stop.store(true, Ordering::Relaxed);
		let _ = self.reader.join();
	}
}

fn pump_menu<S, F>(
	terminal: &mut DefaultTerminal,
	menu: &mut StartMenu,
	events: &Events,
	open: &mut F,
) -> Result<Option<S>, RunError>
where
	F: FnMut(&Path) -> Result<S, String>,
{
	let mut dirty = true;
	let mut captured = menu.mouse_capture();
	let mut next_frame = Instant::now();

	loop {
		match events.recv_timeout(next_frame.saturating_duration_since(Instant::now())) {
			Ok(first) => {
				menu.handle_batch(&gather(first?, events)?);
				dirty = true;
			}
			Err(RecvTimeoutError::Timeout) => {}
			Err(RecvTimeoutError::Disconnected) => return Err(RunError::InputClosed),
		}

		if menu.should_quit() {
			return Ok(None);
		}

		if menu.mouse_capture() != captured {
			captured = apply_mouse_capture(menu.mouse_capture(), captured);
			menu.set_mouse_capture(captured);
		}

		if let Some(project) = menu.take_request() {
			terminal.draw(|frame| menu.draw(frame))?;
			menu.stop_checks(&project);
			match open(&project) {
				Ok(source) => {
					menu.opened(&project);
					return Ok(Some(source));
				}
				Err(message) => menu.open_failed(message),
			}
			dirty = true;
		}

		let now = Instant::now();
		if now < next_frame {
			continue;
		}

		dirty |= menu.tick();
		if dirty {
			terminal.draw(|frame| menu.draw(frame))?;
			dirty = false;
		}
		next_frame = now + FRAME;
	}
}

fn pump<S: ArchiveSource>(terminal: &mut DefaultTerminal, app: &mut App<S>, events: &Events) -> Result<(), RunError> {
	let mut dirty = true;
	let mut captured = app.mouse_capture();
	let mut next_frame = Instant::now();
	let mut next_spin = Instant::now();

	loop {
		match events.recv_timeout(next_frame.saturating_duration_since(Instant::now())) {
			Ok(first) => {
				app.handle_batch(&gather(first?, events)?);
				dirty = true;
			}
			Err(RecvTimeoutError::Timeout) => {}
			Err(RecvTimeoutError::Disconnected) => return Err(RunError::InputClosed),
		}

		if app.should_quit() {
			return Ok(());
		}

		if app.mouse_capture() != captured {
			captured = apply_mouse_capture(app.mouse_capture(), captured);
			app.set_mouse_capture(captured);
		}

		let now = Instant::now();
		if now < next_frame {
			continue;
		}

		dirty |= app.refresh();
		if let Some(text) = app.take_clipboard() {
			copy_to_clipboard(&text);
		}
		if app.spinner_visible() && now >= next_spin {
			app.advance_spinner();
			next_spin = now + SPINNER_STEP;
			dirty = true;
		}

		if dirty {
			terminal.draw(|frame| app.draw(frame))?;
			dirty = false;
		}
		next_frame = now + FRAME;
	}
}

/// OSC 52 asks the terminal to set the clipboard. Most terminals do; one that doesn't ignores the sequence, and the
/// notice with the path is still on screen.
fn osc52(text: &str) -> String {
	format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
}

fn copy_to_clipboard(text: &str) {
	let mut stdout = io::stdout();
	let _ = stdout.write_all(osc52(text).as_bytes());
	let _ = stdout.flush();
}

/// Collects the events that follow the first one closely. A paste arrives as a quick run of key presses, and the app
/// can only keep it out of the shortcuts when it sees the whole run at once.
fn gather(first: (Event, Instant), events: &Events) -> io::Result<Vec<(Event, Instant)>> {
	let mut batch = vec![first];
	while let Ok(next) = events.recv_timeout(BURST_GAP) {
		batch.push(next?);
	}
	Ok(batch)
}

/// Returns the mode in effect: a terminal that refuses the change keeps its current one.
fn apply_mouse_capture(wanted: bool, captured: bool) -> bool {
	let applied = if wanted {
		execute!(io::stdout(), EnableMouseCapture)
	} else {
		execute!(io::stdout(), DisableMouseCapture)
	};

	if applied.is_ok() { wanted } else { captured }
}

fn spawn_input(stop: Arc<AtomicBool>) -> (Events, JoinHandle<()>) {
	let (sender, receiver) = mpsc::channel();
	let reader = thread::spawn(move || {
		while !stop.load(Ordering::Relaxed) {
			let next = match event::poll(INPUT_POLL) {
				Ok(false) => continue,
				Ok(true) => event::read().map(|event| (event, Instant::now())),
				Err(err) => Err(err),
			};

			let failed = next.is_err();
			if sender.send(next).is_err() || failed {
				return;
			}
		}
	});
	(receiver, reader)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_clipboard_sequence_carries_the_text_in_base64() {
		assert_eq!(osc52(r"C:\Photos"), "\x1b]52;c;QzpcUGhvdG9z\x07");
	}
}
