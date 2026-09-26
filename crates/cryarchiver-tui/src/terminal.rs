use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use ratatui::crossterm::execute;

use crate::app::App;
use crate::source::ArchiveSource;
use crate::theme::ColorDepth;

const FRAME: Duration = Duration::from_millis(33);
const SPINNER_STEP: Duration = Duration::from_millis(100);
const INPUT_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum RunError {
	#[error("the TUI needs an interactive terminal; run it from a terminal window, or pass files to use the CLI")]
	NotATerminal,
	#[error("terminal error: {0}")]
	Io(#[from] io::Error),
}

pub fn run<S: ArchiveSource>(source: S) -> Result<(), RunError> {
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Err(RunError::NotATerminal);
	}

	let mut terminal = ratatui::try_init()?;
	release_mouse_on_panic();
	let mut mouse_captured = execute!(io::stdout(), EnableMouseCapture).is_ok();

	let result = event_loop(
		&mut terminal,
		App::new(source, ColorDepth::detect()),
		&mut mouse_captured,
	);

	if mouse_captured {
		let _ = execute!(io::stdout(), DisableMouseCapture);
	}
	ratatui::restore();
	result
}

fn release_mouse_on_panic() {
	let restore_terminal = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		let _ = execute!(io::stdout(), DisableMouseCapture);
		restore_terminal(info);
	}));
}

fn event_loop<S: ArchiveSource>(
	terminal: &mut DefaultTerminal,
	mut app: App<S>,
	mouse_captured: &mut bool,
) -> Result<(), RunError> {
	let stop = Arc::new(AtomicBool::new(false));
	let (events, reader) = spawn_input(stop.clone());

	let result = pump(terminal, &mut app, &events, mouse_captured);

	stop.store(true, Ordering::Relaxed);
	let _ = reader.join();
	result
}

fn pump<S: ArchiveSource>(
	terminal: &mut DefaultTerminal,
	app: &mut App<S>,
	events: &Receiver<io::Result<Event>>,
	mouse_captured: &mut bool,
) -> Result<(), RunError> {
	let mut dirty = true;
	let mut next_frame = Instant::now();
	let mut next_spin = Instant::now();

	loop {
		match events.recv_timeout(next_frame.saturating_duration_since(Instant::now())) {
			Ok(event) => {
				app.handle_event(&event?, Instant::now());
				while let Ok(event) = events.try_recv() {
					app.handle_event(&event?, Instant::now());
				}
				dirty = true;
			}
			Err(RecvTimeoutError::Timeout) => {}
			Err(RecvTimeoutError::Disconnected) => return Ok(()),
		}

		if app.should_quit() {
			return Ok(());
		}

		if app.mouse_capture() != *mouse_captured {
			let command = if app.mouse_capture() {
				execute!(io::stdout(), EnableMouseCapture)
			} else {
				execute!(io::stdout(), DisableMouseCapture)
			};
			command?;
			*mouse_captured = app.mouse_capture();
		}

		let now = Instant::now();
		if now < next_frame {
			continue;
		}

		dirty |= app.refresh();
		if now >= next_spin {
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

/// Crossterm's blocking read runs on its own thread, so the UI thread only ever waits on the channel.
fn spawn_input(stop: Arc<AtomicBool>) -> (Receiver<io::Result<Event>>, JoinHandle<()>) {
	let (sender, receiver) = mpsc::channel();
	let reader = thread::spawn(move || {
		while !stop.load(Ordering::Relaxed) {
			let next = match event::poll(INPUT_POLL) {
				Ok(false) => continue,
				Ok(true) => event::read(),
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
