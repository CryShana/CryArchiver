use std::io::{self, BufWriter, IsTerminal, Stdout, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
	self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyEventKind,
	MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::{Frame, Terminal};

use crate::app::{App, BURST_GAP};
use crate::recent::RecentStore;
use crate::source::ArchiveSource;
use crate::start::StartMenu;
use crate::theme::ColorDepth;

const FRAME: Duration = Duration::from_millis(33);
const WHEEL_FRAME: Duration = Duration::from_millis(16);
const SPINNER_STEP: Duration = Duration::from_millis(100);
const INPUT_POLL: Duration = Duration::from_millis(100);
const PASTE_POLL: Duration = Duration::from_millis(1);
const OUTPUT_BUFFER: usize = 1 << 16;

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
	let mut screen = match start_terminal() {
		Ok(screen) => screen,
		Err(err) => {
			app.shutdown();
			return Err(err);
		}
	};
	app.set_mouse_capture(capture_mouse());

	let input = Input::spawn();
	let result = pump(&mut screen, &mut app, &input.events);
	input.stop();

	finish(&mut screen, app, result)
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
	let mut screen = start_terminal()?;
	menu.set_mouse_capture(capture_mouse());

	let input = Input::spawn();
	let source = match pump_menu(&mut screen, &mut menu, &input.events, &mut open) {
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
	let result = pump(&mut screen, &mut app, &input.events);
	input.stop();

	finish(&mut screen, app, result)
}

fn start_terminal() -> Result<Screen<BufWriter<Stdout>>, RunError> {
	let terminal = open_terminal().inspect_err(|_| ratatui::restore())?;

	release_input_modes_on_panic();
	let _ = execute!(io::stdout(), EnableBracketedPaste);
	Ok(Screen::new(terminal))
}

/// Stdout's own buffer passes output on to the console a kilobyte at a time; a frame goes out in one write instead.
fn open_terminal() -> io::Result<Terminal<CrosstermBackend<BufWriter<Stdout>>>> {
	enable_raw_mode()?;
	execute!(io::stdout(), EnterAlternateScreen)?;

	let output = BufWriter::with_capacity(OUTPUT_BUFFER, io::stdout());
	Terminal::new(CrosstermBackend::new(output))
}

fn capture_mouse() -> bool {
	execute!(io::stdout(), EnableMouseCapture).is_ok()
}

fn finish<W: Write, S: ArchiveSource>(
	screen: &mut Screen<W>,
	mut app: App<S>,
	result: Result<(), RunError>,
) -> Result<(), RunError> {
	app.begin_shutdown();
	let _ = screen.draw(|frame| app.draw(frame));
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
	let report = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		release_input_modes();
		ratatui::restore();
		report(info);
	}));
}

/// The terminal, sent only frames that differ from the one on screen.
struct Screen<W: Write> {
	terminal: Terminal<CrosstermBackend<W>>,
	shown: Option<Buffer>,
}

impl<W: Write> Screen<W> {
	fn new(terminal: Terminal<CrosstermBackend<W>>) -> Self {
		Self { terminal, shown: None }
	}

	/// Returns whether the frame was written. A frame like the one on screen writes nothing at all, where the
	/// terminal would still send the cursor and the colour resets that end every frame.
	fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<bool> {
		self.terminal.autoresize()?;
		render(&mut self.terminal.get_frame());

		let rendered = self.terminal.current_buffer_mut();
		if self.shown.as_ref() == Some(rendered) {
			rendered.reset();
			return Ok(false);
		}

		match &mut self.shown {
			Some(shown) => {
				shown.area = rendered.area;
				shown.content.clone_from(&rendered.content);
			}
			None => self.shown = Some(rendered.clone()),
		}
		self.terminal.apply_buffer()?;
		Ok(true)
	}
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

/// How soon an input event needs a frame. Wheel events arrive in floods, so they share frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Redraw {
	Never,
	Paced,
	Now,
}

/// Mouse moves, releases and the like change nothing on screen.
fn redraw(event: &Event) -> Redraw {
	match event {
		Event::Key(key) if key.kind == KeyEventKind::Press => Redraw::Now,
		Event::Mouse(mouse) => match mouse.kind {
			MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => Redraw::Paced,
			MouseEventKind::Down(_) => Redraw::Now,
			_ => Redraw::Never,
		},
		Event::Paste(_) | Event::Resize(..) => Redraw::Now,
		Event::Key(_) | Event::FocusGained | Event::FocusLost => Redraw::Never,
	}
}

/// When the next frame is due: every [`FRAME`] for the source's changes, at once for input, and for the wheel no
/// sooner than [`WHEEL_FRAME`] after the last frame.
#[derive(Debug, Clone, Copy)]
struct Pacer {
	next: Instant,
	last: Option<Instant>,
}

impl Pacer {
	fn new(now: Instant) -> Self {
		Self { next: now, last: None }
	}

	fn wait(&self, now: Instant) -> Duration {
		self.next.saturating_duration_since(now)
	}

	fn due(&self, now: Instant) -> bool {
		now >= self.next
	}

	/// Returns whether the input can have changed the screen.
	fn input(&mut self, batch: &[(Event, Instant)], now: Instant) -> bool {
		match batch.iter().map(|(event, _)| redraw(event)).max() {
			Some(Redraw::Now) => self.next = now,
			Some(Redraw::Paced) => self.next = self.next.min(self.last.map_or(now, |last| last + WHEEL_FRAME)),
			Some(Redraw::Never) | None => return false,
		}
		true
	}

	fn framed(&mut self, now: Instant) {
		self.last = Some(now);
		self.next = now + FRAME;
	}
}

fn pump_menu<W, S, F>(
	screen: &mut Screen<W>,
	menu: &mut StartMenu,
	events: &Events,
	open: &mut F,
) -> Result<Option<S>, RunError>
where
	W: Write,
	F: FnMut(&Path) -> Result<S, String>,
{
	let mut dirty = true;
	let mut captured = menu.mouse_capture();
	let mut pacer = Pacer::new(Instant::now());

	loop {
		match events.recv_timeout(pacer.wait(Instant::now())) {
			Ok(first) => {
				let batch = gather(first?, events)?;
				menu.handle_batch(&batch);
				dirty |= pacer.input(&batch, Instant::now());
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
			screen.draw(|frame| menu.draw(frame))?;
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
		if !pacer.due(now) {
			continue;
		}

		dirty |= menu.tick();
		if dirty {
			screen.draw(|frame| menu.draw(frame))?;
			dirty = false;
		}
		pacer.framed(now);
	}
}

fn pump<W: Write, S: ArchiveSource>(screen: &mut Screen<W>, app: &mut App<S>, events: &Events) -> Result<(), RunError> {
	let mut dirty = true;
	let mut captured = app.mouse_capture();
	let mut pacer = Pacer::new(Instant::now());
	let mut next_spin = Instant::now();

	loop {
		match events.recv_timeout(pacer.wait(Instant::now())) {
			Ok(first) => {
				let batch = gather(first?, events)?;
				app.handle_batch(&batch);
				dirty |= pacer.input(&batch, Instant::now());
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
		if !pacer.due(now) {
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
			screen.draw(|frame| app.draw(frame))?;
			dirty = false;
		}
		pacer.framed(now);
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

/// Collects the events already waiting behind the first one. A paste arrives as a quick run of key presses, and the
/// app can only keep it out of the shortcuts when it sees the whole run at once, so after a key press it also waits
/// out [`BURST_GAP`] for the next one. Sleeping in short steps keeps that wait close to the gap, where a channel
/// timeout on Windows lasts a whole timer tick of about 16 ms. A batch never gathers longer than a frame.
fn gather(first: (Event, Instant), events: &Events) -> io::Result<Vec<(Event, Instant)>> {
	let started = Instant::now();
	let mut batch = vec![first];

	loop {
		match events.try_recv() {
			Ok(next) => {
				batch.push(next?);
				continue;
			}
			Err(TryRecvError::Empty) => {}
			Err(TryRecvError::Disconnected) => break,
		}

		let Some(until) = paste_window(&batch) else { break };
		let now = Instant::now();
		if now >= until || now.duration_since(started) >= FRAME {
			break;
		}
		thread::sleep(PASTE_POLL.min(until - now));
	}
	Ok(batch)
}

/// Until when the next key of a paste may still arrive: [`BURST_GAP`] after the last event, while that is a key press.
fn paste_window(batch: &[(Event, Instant)]) -> Option<Instant> {
	match batch.last() {
		Some((Event::Key(key), at)) if key.kind == KeyEventKind::Press => Some(*at + BURST_GAP),
		_ => None,
	}
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
	use std::sync::{Arc, Mutex};

	use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent};
	use ratatui::layout::Rect;
	use ratatui::{TerminalOptions, Viewport};

	use super::*;
	use crate::mock::MockSource;
	use crate::source::Action;
	use crate::view::{ArchiveView, FileDetails, FileId, FileRow, FilterResult, IssueId, NodeId, Notification};

	const WIDTH: u16 = 160;
	const HEIGHT: u16 = 45;

	fn key(code: KeyCode) -> Event {
		Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
	}

	fn released(code: KeyCode) -> Event {
		let mut key = KeyEvent::new(code, KeyModifiers::NONE);
		key.kind = KeyEventKind::Release;
		Event::Key(key)
	}

	fn mouse(kind: MouseEventKind) -> Event {
		Event::Mouse(MouseEvent {
			kind,
			column: 20,
			row: 20,
			modifiers: KeyModifiers::NONE,
		})
	}

	/// Each flush with the bytes written since the one before.
	#[derive(Clone, Default)]
	struct Recorder {
		flushes: Arc<Mutex<Vec<(Instant, usize)>>>,
		pending: usize,
	}

	impl Write for Recorder {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			self.pending += buf.len();
			Ok(buf.len())
		}

		fn flush(&mut self) -> io::Result<()> {
			if self.pending > 0 {
				self.flushes.lock().unwrap().push((Instant::now(), self.pending));
				self.pending = 0;
			}
			Ok(())
		}
	}

	impl Recorder {
		fn written(&self) -> usize {
			self.flushes.lock().unwrap().iter().map(|&(_, bytes)| bytes).sum()
		}
	}

	fn screen(recorder: &Recorder) -> Screen<Recorder> {
		let options = TerminalOptions {
			viewport: Viewport::Fixed(Rect::new(0, 0, WIDTH, HEIGHT)),
		};
		Screen::new(Terminal::with_options(CrosstermBackend::new(recorder.clone()), options).unwrap())
	}

	#[test]
	fn the_clipboard_sequence_carries_the_text_in_base64() {
		assert_eq!(osc52(r"C:\Photos"), "\x1b]52;c;QzpcUGhvdG9z\x07");
	}

	#[test]
	fn only_input_that_can_change_the_screen_asks_for_a_frame() {
		assert_eq!(redraw(&key(KeyCode::Down)), Redraw::Now);
		assert_eq!(redraw(&mouse(MouseEventKind::Down(MouseButton::Right))), Redraw::Now);
		assert_eq!(redraw(&Event::Paste("text".into())), Redraw::Now);
		assert_eq!(redraw(&Event::Resize(80, 24)), Redraw::Now);
		assert_eq!(redraw(&mouse(MouseEventKind::ScrollDown)), Redraw::Paced);
		assert_eq!(redraw(&mouse(MouseEventKind::ScrollUp)), Redraw::Paced);

		assert_eq!(redraw(&mouse(MouseEventKind::Moved)), Redraw::Never);
		assert_eq!(redraw(&mouse(MouseEventKind::Up(MouseButton::Left))), Redraw::Never);
		assert_eq!(redraw(&mouse(MouseEventKind::Drag(MouseButton::Left))), Redraw::Never);
		assert_eq!(redraw(&released(KeyCode::Down)), Redraw::Never);
		assert_eq!(redraw(&Event::FocusLost), Redraw::Never);
	}

	#[test]
	fn a_key_press_is_due_at_once_even_right_after_a_frame() {
		let start = Instant::now();
		let mut pacer = Pacer::new(start);
		pacer.framed(start);

		let now = start + Duration::from_millis(1);
		assert!(pacer.input(&[(key(KeyCode::Down), now)], now));
		assert!(pacer.due(now));
	}

	#[test]
	fn the_wheel_shares_frames_but_never_waits_longer_than_the_frame_due_anyway() {
		let start = Instant::now();
		let mut pacer = Pacer::new(start);
		pacer.framed(start);

		let now = start + Duration::from_millis(1);
		assert!(pacer.input(&[(mouse(MouseEventKind::ScrollDown), now)], now));
		assert!(!pacer.due(now));
		assert!(pacer.due(start + WHEEL_FRAME));

		let mut pacer = Pacer::new(start);
		pacer.framed(start);
		let late = start + FRAME + WHEEL_FRAME;
		assert!(pacer.input(&[(mouse(MouseEventKind::ScrollDown), late)], late));
		assert!(pacer.due(start + FRAME), "the source's frame stays due");
	}

	#[test]
	fn mouse_moves_leave_the_frames_to_the_source() {
		let start = Instant::now();
		let mut pacer = Pacer::new(start);
		pacer.framed(start);

		let moves: Vec<_> = (0..20)
			.map(|_| (mouse(MouseEventKind::Moved), start))
			.chain([(released(KeyCode::Down), start)])
			.collect();
		assert!(!pacer.input(&moves, start));
		assert!(!pacer.due(start + FRAME.saturating_sub(Duration::from_millis(1))));
		assert!(pacer.due(start + FRAME));
	}

	#[test]
	fn a_batch_takes_every_waiting_event_and_waits_only_after_a_key_press() {
		let (sender, events) = mpsc::channel();
		let now = Instant::now();
		for _ in 0..50 {
			sender.send(Ok((mouse(MouseEventKind::ScrollDown), now))).unwrap();
		}

		let batch = gather((mouse(MouseEventKind::ScrollDown), now), &events).unwrap();
		assert_eq!(batch.len(), 51);
		assert_eq!(paste_window(&batch), None);

		let pressed = vec![(key(KeyCode::Char('a')), now)];
		assert_eq!(paste_window(&pressed), Some(now + BURST_GAP));
		let then_moved = vec![(key(KeyCode::Char('a')), now), (mouse(MouseEventKind::Moved), now)];
		assert_eq!(paste_window(&then_moved), None);
	}

	#[test]
	fn keys_queued_together_stay_in_one_batch() {
		let (sender, events) = mpsc::channel();
		let typed = Instant::now();
		for (index, c) in "pasted".chars().enumerate().skip(1) {
			let at = typed + Duration::from_micros(100) * u32::try_from(index).unwrap();
			sender.send(Ok((key(KeyCode::Char(c)), at))).unwrap();
		}

		let batch = gather((key(KeyCode::Char('p')), typed), &events).unwrap();
		assert_eq!(batch.len(), 6);
	}

	#[test]
	fn a_frame_like_the_one_on_screen_writes_nothing() {
		let recorder = Recorder::default();
		let mut screen = screen(&recorder);
		let text = |text: &'static str| {
			move |frame: &mut Frame| frame.buffer_mut().set_string(0, 0, text, ratatui::style::Style::new())
		};

		assert!(screen.draw(text("one")).unwrap());
		let after_first = recorder.written();
		assert!(after_first > 0);

		assert!(!screen.draw(text("one")).unwrap());
		assert_eq!(recorder.written(), after_first);

		assert!(screen.draw(text("two")).unwrap());
		assert!(recorder.written() > after_first);
		assert!(!screen.draw(text("two")).unwrap());
		assert!(
			screen.draw(|_| {}).unwrap(),
			"a skipped frame leaves nothing behind for the next one"
		);
	}

	/// The demo stopped mid-run, its scan done: nothing changes unless the user does something.
	struct Still(MockSource, ArchiveView);

	impl Still {
		fn new() -> Self {
			let source = MockSource::new(7);
			let mut view = source.view().clone();
			view.overall.scan_complete = true;
			Self(source, view)
		}
	}

	impl ArchiveSource for Still {
		fn refresh(&mut self) -> bool {
			false
		}

		fn view(&self) -> &ArchiveView {
			&self.1
		}

		fn files(&self, folder: NodeId) -> Option<&[FileRow]> {
			self.0.files(folder)
		}

		fn rows_version(&self) -> u64 {
			self.0.rows_version()
		}

		fn filter(&self) -> Option<&FilterResult> {
			self.0.filter()
		}

		fn details(&self, file: FileId) -> Option<FileDetails> {
			self.0.details(file)
		}

		fn issue_log(&self, issue: IssueId, attempt: u32) -> Option<Vec<String>> {
			self.0.issue_log(issue, attempt)
		}

		fn take_notifications(&mut self) -> Vec<Notification> {
			self.0.take_notifications()
		}

		fn perform(&mut self, action: Action) {
			self.0.perform(action);
		}

		fn shutdown(&mut self) {}
	}

	type Feed = mpsc::Sender<io::Result<(Event, Instant)>>;

	struct Pumped {
		sent: Vec<Instant>,
		flushes: Vec<(Instant, usize)>,
		shown: Option<Buffer>,
	}

	/// Runs the real loop on the demo until `feed` returns; gives back what `feed` sent, every frame written and the
	/// last one.
	fn run_pump(feed: impl FnOnce(&Feed) -> Vec<Instant> + Send + 'static) -> Pumped {
		let recorder = Recorder::default();
		let mut screen = screen(&recorder);
		let mut app = App::new(Still::new(), ColorDepth::TrueColor);
		let (sender, events) = mpsc::channel();

		let feeder = thread::spawn(move || feed(&sender));
		let _ = pump(&mut screen, &mut app, &events);
		let sent = feeder.join().unwrap();
		let flushes = recorder.flushes.lock().unwrap().clone();
		Pumped {
			sent,
			flushes,
			shown: screen.shown,
		}
	}

	/// 20 wheel steps of 3 rows stay short of the end of the demo's Library, so each one shows.
	#[test]
	fn every_wheel_event_reaches_the_screen_however_the_loop_batches_them() {
		const EVENTS: usize = 30;

		let wheel = |index: usize| {
			if index.is_multiple_of(3) {
				mouse(MouseEventKind::Moved)
			} else {
				mouse(MouseEventKind::ScrollDown)
			}
		};
		let one_at_a_time = |events: usize| {
			let mut app = App::new(Still::new(), ColorDepth::TrueColor);
			let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(WIDTH, HEIGHT)).unwrap();
			terminal.draw(|frame| app.draw(frame)).unwrap();
			for index in 0..events {
				app.handle_event(&wheel(index), Instant::now());
			}
			terminal.draw(|frame| app.draw(frame)).unwrap();
			terminal.backend().buffer().clone()
		};

		let pumped = run_pump(move |sender| {
			thread::sleep(Duration::from_millis(50));
			for index in 0..EVENTS {
				sender.send(Ok((wheel(index), Instant::now()))).unwrap();
			}
			thread::sleep(Duration::from_millis(100));
			Vec::new()
		});

		let shown = pumped.shown.unwrap();
		assert_eq!(shown, one_at_a_time(EVENTS));
		assert_ne!(
			shown,
			one_at_a_time(EVENTS - 1),
			"one wheel step fewer shows other rows"
		);
	}

	mod latency {
		use super::*;

		fn ms(duration: Duration) -> f64 {
			duration.as_secs_f64() * 1000.0
		}

		/// From a key press entering the input channel to the first frame written after it, through the real loop.
		#[test]
		#[ignore = "timing benchmark; run in release with --ignored --nocapture"]
		fn key_press_to_frame_latency() {
			let Pumped { sent, flushes, .. } = run_pump(|sender| {
				let mut sent = Vec::new();
				for press in 0..40u64 {
					thread::sleep(Duration::from_millis(40 + press * 7 % 50));
					let code = if press.is_multiple_of(2) {
						KeyCode::Down
					} else {
						KeyCode::Up
					};
					let at = Instant::now();
					sender.send(Ok((key(code), at))).unwrap();
					sent.push(at);
				}
				thread::sleep(Duration::from_millis(100));
				sent
			});

			let mut latencies: Vec<Duration> = sent
				.iter()
				.filter_map(|&at| {
					let &(written, _) = flushes.iter().find(|&&(written, bytes)| written >= at && bytes > 200)?;
					Some(written - at)
				})
				.collect();
			latencies.sort();
			println!(
				"key press to frame: median {:.2} ms, max {:.2} ms over {} presses",
				ms(latencies[latencies.len() / 2]),
				ms(latencies[latencies.len() - 1]),
				latencies.len()
			);
		}

		/// A wheel spun for a second, an event every 4 ms, while the mouse moves: how many frames show it while it
		/// spins, and how long the screen takes to catch up once it stops.
		#[test]
		#[ignore = "timing benchmark; run in release with --ignored --nocapture"]
		fn wheel_flood_frames_and_catch_up() {
			let Pumped { sent, flushes, .. } = run_pump(|sender| {
				thread::sleep(Duration::from_millis(200));
				let mut sent = Vec::new();
				let start = Instant::now();
				for index in 0..250 {
					while Instant::now() < start + Duration::from_millis(4) * index {
						std::hint::spin_loop();
					}
					let kind = if (index / 10).is_multiple_of(2) {
						MouseEventKind::ScrollDown
					} else {
						MouseEventKind::ScrollUp
					};
					let at = Instant::now();
					sender.send(Ok((mouse(MouseEventKind::Moved), at))).unwrap();
					sender.send(Ok((mouse(kind), at))).unwrap();
					sent.push(at);
				}
				thread::sleep(Duration::from_millis(300));
				sent
			});

			let (first, last) = (sent[0], *sent.last().unwrap());
			let during = flushes
				.iter()
				.filter(|&&(at, bytes)| at > first && at <= last && bytes > 200)
				.count();
			let caught_up = flushes.iter().rev().find(|&&(_, bytes)| bytes > 200).map(|&(at, _)| at);
			println!(
				"wheel flood over {:.0} ms: {during} frames while spinning, last frame {:.2} ms after the last event",
				ms(last - first),
				caught_up.map_or(f64::NAN, |at| ms(at.saturating_duration_since(last)))
			);
		}
	}
}
