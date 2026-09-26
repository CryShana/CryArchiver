mod inputs;
mod report;

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use clap::{Args, Parser, Subcommand};
use cryarchiver_engine::commit::{OutputSink, check_space};
use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::naming::cli_output_path;
use cryarchiver_engine::outcome::{ErrorKind, JobOutcome};
use cryarchiver_engine::preset::Speed;
use cryarchiver_engine::sink::CliSink;
use cryarchiver_engine::text::display_path;
use cryarchiver_engine::toolchain::{Status, ToolConfig, Toolchain};
use cryarchiver_engine::workspace::sweep;
use tokio::sync::Notify;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::report::{Report, Totals, outcome_line};

const TUI_NOTICE: &str =
	"The archival TUI is not available yet. Run `cryarchiver --help` to compress files from the command line.";

#[derive(Parser)]
#[command(
	name = "cryarchiver",
	version = cryarchiver_engine::VERSION,
	about = "Makes media files smaller without thinking about encoders",
	args_conflicts_with_subcommands = true
)]
struct Cli {
	#[command(subcommand)]
	command: Option<Command>,
	#[command(flatten)]
	compress: CompressArgs,
}

#[derive(Subcommand)]
enum Command {
	#[command(about = "Show which ffmpeg features are available")]
	Doctor {
		#[arg(long, value_name = "PATH", help = "ffmpeg binary or the folder containing it")]
		ffmpeg: Option<PathBuf>,
	},
	#[command(about = "Open the archival TUI")]
	Tui { project: Option<PathBuf> },
}

#[derive(Args)]
struct CompressArgs {
	#[arg(
		value_name = "INPUT",
		help = "Files, folders (not recursive) or wildcards such as *.flac"
	)]
	inputs: Vec<PathBuf>,
	#[arg(
		short,
		long,
		value_name = "DIR",
		help = "Write outputs here instead of next to each source"
	)]
	output: Option<PathBuf>,
	#[arg(short, long, value_enum, default_value_t, help = "Speed preset")]
	speed: Speed,
	#[arg(long, help = "Keep personal metadata and the source's file timestamps")]
	keep_metadata: bool,
	#[arg(long, help = "Replace existing outputs (never sources)")]
	overwrite: bool,
	#[arg(
		long,
		value_name = "PERCENT",
		default_value_t = 10,
		value_parser = clap::value_parser!(u8).range(0..=90),
		help = "Keep the original unless the output is at least this much smaller"
	)]
	min_savings: u8,
	#[arg(short, long, value_name = "N", help = "Files processed in parallel")]
	jobs: Option<usize>,
	#[arg(long, value_name = "PATH", help = "ffmpeg binary or the folder containing it")]
	ffmpeg: Option<PathBuf>,
	#[arg(short, long, help = "Only print failures and the summary")]
	quiet: bool,
}

#[derive(Default)]
struct VideoQueue {
	waiting: AtomicUsize,
	joined: Notify,
}

impl VideoQueue {
	fn waiting(&self) -> usize {
		self.waiting.load(Ordering::SeqCst)
	}

	fn busy(&self, running: usize, parallel: usize) -> usize {
		running.saturating_sub(self.waiting().min(parallel))
	}
}

struct QueueSeat {
	queue: Arc<VideoQueue>,
	waiting: AtomicBool,
}

impl QueueSeat {
	fn new(queue: Arc<VideoQueue>) -> Self {
		Self {
			queue,
			waiting: AtomicBool::new(false),
		}
	}

	fn observe(&self, event: &JobEvent) {
		match event {
			JobEvent::Stage(Stage::Queued) => {
				if !self.waiting.swap(true, Ordering::SeqCst) {
					self.queue.waiting.fetch_add(1, Ordering::SeqCst);
					self.queue.joined.notify_one();
				}
			}
			JobEvent::Stage(_) => self.leave(),
			JobEvent::Progress(_) | JobEvent::Warning(_) => {}
		}
	}

	fn leave(&self) {
		if self.waiting.swap(false, Ordering::SeqCst) {
			self.queue.waiting.fetch_sub(1, Ordering::SeqCst);
		}
	}
}

impl Drop for QueueSeat {
	fn drop(&mut self) {
		self.leave();
	}
}

fn main() -> ExitCode {
	let code = if std::env::args_os().len() == 1 {
		tui_notice()
	} else {
		dispatch(Cli::parse())
	};

	pause_if_launched_from_explorer();
	ExitCode::from(code)
}

fn dispatch(cli: Cli) -> u8 {
	match cli.command {
		Some(Command::Doctor { ffmpeg }) => block_on(doctor(ffmpeg)),
		Some(Command::Tui { .. }) => tui_notice(),
		None if opens_project(&cli.compress.inputs) => tui_notice(),
		None => block_on(compress(cli.compress)),
	}
}

fn block_on<F: Future>(future: F) -> F::Output {
	tokio::runtime::Runtime::new()
		.expect("tokio runtime starts")
		.block_on(future)
}

fn opens_project(inputs: &[PathBuf]) -> bool {
	inputs
		.first()
		.is_some_and(|input| input.extension().is_some_and(|ext| ext == "cryarchive"))
}

fn tui_notice() -> u8 {
	println!("{TUI_NOTICE}");
	0
}

async fn doctor(ffmpeg: Option<PathBuf>) -> u8 {
	let tools = match Toolchain::detect(
		&ToolConfig {
			ffmpeg,
			cache_dir: None,
			refresh: true,
		},
		&CancellationToken::new(),
	)
	.await
	{
		Ok(tools) => tools,
		Err(err) => {
			eprintln!("✗ {err}");
			return 2;
		}
	};

	let lines = tools.report();
	for line in &lines {
		let glyph = match line.status {
			Status::Ok => "✓",
			Status::Warn => "⚠",
			Status::Missing => "✗",
		};
		println!("{glyph} {:<16} {}", line.label, line.detail);
	}

	u8::from(lines.iter().any(|line| line.status == Status::Missing)) * 2
}

async fn compress(args: CompressArgs) -> u8 {
	let inputs = inputs::expand(&args.inputs);
	for note in &inputs.notes {
		eprintln!("note: {note}");
	}

	if inputs.files.is_empty() {
		eprintln!("nothing to do: no input files");
		return 2;
	}

	let cancel = CancellationToken::new();
	let tools = match Toolchain::detect(
		&ToolConfig {
			ffmpeg: args.ffmpeg,
			cache_dir: None,
			refresh: false,
		},
		&cancel,
	)
	.await
	{
		Ok(tools) => tools,
		Err(err) => {
			eprintln!("error: {err}");
			return 2;
		}
	};

	let universal = tools.universal_problems();
	if !universal.is_empty() {
		for problem in &universal {
			eprintln!("error: {problem}");
		}
		eprintln!("run \"cryarchiver doctor\" after changing ffmpeg");
		return 2;
	}

	for line in tools.report().iter().filter(|line| line.status == Status::Missing) {
		let kinds: Vec<String> = line.needed_for.iter().map(ToString::to_string).collect();
		eprintln!(
			"warning: {}: {} · {} files will fail",
			line.label,
			line.detail,
			kinds.join(" and ")
		);
	}

	let files: Vec<PathBuf> = inputs.files.into_iter().map(absolute).collect();
	let sink = Arc::new(CliSink {
		output_dir: args.output.map(absolute),
		overwrite: args.overwrite,
	});
	prepare_work_roots(&files, sink.as_ref());
	cancel_on_ctrl_c(&cancel);

	let min_savings = f64::from(args.min_savings) / 100.0;
	let jobs = files
		.into_iter()
		.map(|source| Job {
			source,
			speed: args.speed,
			keep_metadata: args.keep_metadata,
			min_savings,
		})
		.collect();
	let parallel = args.jobs.unwrap_or_else(default_jobs).max(1);
	let engine = Engine::new(tools).with_video_jobs(args.jobs.unwrap_or(1));
	let report = Report::new(args.quiet);

	let totals = run_all(&Arc::new(engine), &sink, jobs, parallel, &report, &cancel).await;
	let _ = writeln!(io::stdout(), "{}", totals.line());
	totals.exit_code().max(u8::from(inputs.problems > 0))
}

async fn run_all(
	engine: &Arc<Engine>,
	sink: &Arc<CliSink>,
	jobs: Vec<Job>,
	parallel: usize,
	report: &Report,
	cancel: &CancellationToken,
) -> Totals {
	let mut totals = Totals::default();
	let mut running = JoinSet::new();
	let mut queue = jobs.into_iter();
	let mut claimed: HashMap<OsString, PathBuf> = HashMap::new();
	let videos = Arc::new(VideoQueue::default());

	while let Some(job) = queue.next() {
		if cancel.is_cancelled() {
			eprintln!("cancelled: {} files were not started", queue.len() + 1);
			break;
		}

		let key = output_key(&job.source, sink.output_dir.as_deref());
		if let Some(first) = claimed.get(&key) {
			let reason = format!("same output name as {}", display_path(first));
			record(report, &mut totals, &job.source, &JobOutcome::Skipped { reason });
			continue;
		}
		claimed.insert(key, job.source.clone());

		if let Some(existing) = sink.existing_output(&job.source) {
			let reason = format!("output already exists: {}", display_path(&existing));
			record(report, &mut totals, &job.source, &JobOutcome::Skipped { reason });
			continue;
		}

		while videos.busy(running.len(), parallel) >= parallel {
			tokio::select! {
				Some(joined) = running.join_next() => record_joined(report, &mut totals, joined),
				() = videos.joined.notified() => {}
			}
		}

		if cancel.is_cancelled() {
			eprintln!("cancelled: {} files were not started", queue.len() + 1);
			break;
		}

		let bar = report.bar(&job.source);
		let seat = QueueSeat::new(videos.clone());
		let (engine, sink, cancel) = (engine.clone(), sink.clone(), cancel.clone());
		running.spawn(async move {
			let on_event = |event: JobEvent| {
				seat.observe(&event);
				report::apply(&bar, &event);
			};
			let outcome = engine.run(&job, sink.as_ref(), &on_event, &cancel).await;
			bar.finish_and_clear();
			(job.source, outcome)
		});
	}

	while let Some(joined) = running.join_next().await {
		record_joined(report, &mut totals, joined);
	}
	totals
}

fn output_key(source: &Path, output_dir: Option<&Path>) -> OsString {
	let key = cli_output_path(source, output_dir, "").into_os_string();
	if cfg!(windows) {
		key.to_string_lossy().to_lowercase().into()
	} else {
		key
	}
}

fn absolute(path: PathBuf) -> PathBuf {
	std::path::absolute(&path).unwrap_or(path)
}

fn record(report: &Report, totals: &mut Totals, source: &Path, outcome: &JobOutcome) {
	report.print(&outcome_line(source, outcome), matches!(outcome, JobOutcome::Failed(_)));
	totals.add(outcome);
}

fn record_joined(report: &Report, totals: &mut Totals, joined: Result<(PathBuf, JobOutcome), JoinError>) {
	match joined {
		Ok((source, outcome)) => record(report, totals, &source, &outcome),
		Err(err) => {
			report.print(&format!("✗ internal error: {err}"), true);
			totals.add(&JobOutcome::Failed(ErrorKind::EncodeFailed(err.to_string()).into()));
		}
	}
}

fn prepare_work_roots(files: &[PathBuf], sink: &dyn OutputSink) {
	let mut per_root: BTreeMap<PathBuf, u64> = BTreeMap::new();
	for file in files {
		*per_root.entry(sink.work_root(file)).or_default() += fs::metadata(file).map_or(0, |meta| meta.len());
	}

	for (root, total) in &per_root {
		sweep(root);

		if let Err(err) = check_space(root, *total) {
			eprintln!(
				"warning: {err} near {}; files that don't fit will fail and can be retried",
				display_path(root.parent().unwrap_or(root))
			);
		}
	}
}

fn cancel_on_ctrl_c(cancel: &CancellationToken) {
	let cancel = cancel.clone();
	tokio::spawn(async move {
		if tokio::signal::ctrl_c().await.is_err() {
			return;
		}

		cancel.cancel();

		// Exiting here is safe: the job object and the parent-death signal end the tools, and the next run's sweep
		// removes the leftover workspaces.
		if tokio::signal::ctrl_c().await.is_ok() {
			std::process::exit(130);
		}
	});
}

fn default_jobs() -> usize {
	std::thread::available_parallelism().map_or(2, |cores| (cores.get() / 2).clamp(1, 4))
}

#[cfg(windows)]
fn pause_if_launched_from_explorer() {
	use std::io::{BufRead, IsTerminal};

	use windows::Win32::System::Console::GetConsoleProcessList;

	let mut processes = [0u32; 4];
	// SAFETY: the buffer is valid for the duration of the call.
	let attached = unsafe { GetConsoleProcessList(&mut processes) };
	if attached == 1 && std::io::stdin().is_terminal() {
		println!("\nPress Enter to close.");
		let _ = std::io::stdin().lock().read_line(&mut String::new());
	}
}

#[cfg(not(windows))]
fn pause_if_launched_from_explorer() {}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	#[test]
	fn at_most_one_batch_of_queued_videos_is_discounted() {
		let videos = VideoQueue::default();
		videos.waiting.store(10, Ordering::SeqCst);

		assert_eq!(videos.busy(12, 4), 8);
		assert_eq!(videos.busy(3, 4), 0);
	}

	#[test]
	fn queued_videos_count_until_they_move_on_or_end() {
		let videos = Arc::new(VideoQueue::default());
		let first = QueueSeat::new(videos.clone());
		let second = QueueSeat::new(videos.clone());

		first.observe(&JobEvent::Stage(Stage::Probe));
		assert_eq!(videos.waiting(), 0);

		first.observe(&JobEvent::Stage(Stage::Queued));
		first.observe(&JobEvent::Stage(Stage::Queued));
		second.observe(&JobEvent::Stage(Stage::Queued));
		first.observe(&JobEvent::Progress(0.5));
		assert_eq!(videos.waiting(), 2);

		first.observe(&JobEvent::Stage(Stage::Analyze));
		assert_eq!(videos.waiting(), 1);

		drop(second);
		assert_eq!(videos.waiting(), 0);
	}

	#[tokio::test]
	async fn entering_the_queue_wakes_the_scheduler() {
		let videos = Arc::new(VideoQueue::default());
		let seat = QueueSeat::new(videos.clone());

		seat.observe(&JobEvent::Stage(Stage::Queued));

		tokio::time::timeout(Duration::from_secs(1), videos.joined.notified())
			.await
			.expect("the wake-up is stored until the scheduler waits");
	}
}
