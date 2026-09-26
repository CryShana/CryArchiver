use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
	self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
};
use ratatui::crossterm::execute;

use crate::app::{App, BURST_GAP};
use crate::source::ArchiveSource;
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

/// Runs the TUI until the user quits. The source is shut down while a "stopping" frame is on screen and is dropped
/// only after the terminal has been restored, because stopping a real run can take seconds.
pub fn run<S: ArchiveSource>(source: S) -> Result<(), RunError> {
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Err(RunError::NotATerminal);
	}

	let mut app = App::new(source, ColorDepth::detect());
	let mut terminal = match ratatui::try_init() {
		Ok(terminal) => terminal,
		Err(err) => {
			ratatui::restore();
			app.shutdown();
			return Err(err.into());
		}
	};

	release_input_modes_on_panic();
	let _ = execute!(io::stdout(), EnableBracketedPaste);
	let captured = execute!(io::stdout(), EnableMouseCapture).is_ok();
	app.set_mouse_capture(captured);

	let result = event_loop(&mut terminal, &mut app);

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

fn event_loop<S: ArchiveSource>(terminal: &mut DefaultTerminal, app: &mut App<S>) -> Result<(), RunError> {
	let stop = Arc::new(AtomicBool::new(false));
	let (events, reader) = spawn_input(stop.clone());

	let result = pump(terminal, app, &events);

	stop.store(true, Ordering::Relaxed);
	let _ = reader.join();
	result
}

fn pump<S: ArchiveSource>(
	terminal: &mut DefaultTerminal,
	app: &mut App<S>,
	events: &Receiver<io::Result<(Event, Instant)>>,
) -> Result<(), RunError> {
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
			captured = apply_mouse_capture(app, captured);
		}

		let now = Instant::now();
		if now < next_frame {
			continue;
		}

		dirty |= app.refresh();
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

/// Collects the events that follow the first one closely. A paste arrives as a quick run of key presses, and the app
/// can only keep it out of the shortcuts when it sees the whole run at once.
fn gather(
	first: (Event, Instant),
	events: &Receiver<io::Result<(Event, Instant)>>,
) -> io::Result<Vec<(Event, Instant)>> {
	let mut batch = vec![first];
	while let Ok(next) = events.recv_timeout(BURST_GAP) {
		batch.push(next?);
	}
	Ok(batch)
}

/// A terminal that refuses the change keeps its current mode, and the app is told so.
fn apply_mouse_capture<S: ArchiveSource>(app: &mut App<S>, captured: bool) -> bool {
	let wanted = app.mouse_capture();
	let applied = if wanted {
		execute!(io::stdout(), EnableMouseCapture)
	} else {
		execute!(io::stdout(), DisableMouseCapture)
	};

	if applied.is_ok() {
		wanted
	} else {
		app.set_mouse_capture(captured);
		captured
	}
}

/// Crossterm's blocking read runs on its own thread, so the UI thread only ever waits on the channel.
fn spawn_input(stop: Arc<AtomicBool>) -> (Receiver<io::Result<(Event, Instant)>>, JoinHandle<()>) {
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
