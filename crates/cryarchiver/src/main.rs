mod headless;
mod inputs;
mod projects;
mod report;

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use clap::{Args, CommandFactory, Parser, Subcommand};
use cryarchiver_engine::commit::{OutputSink, check_space};
use cryarchiver_engine::engine::{Engine, Job, JobEvent, Stage};
use cryarchiver_engine::format::OutputFormat;
use cryarchiver_engine::naming::cli_output_path;
use cryarchiver_engine::options::JobOptions;
use cryarchiver_engine::outcome::{ErrorKind, JobOutcome};
use cryarchiver_engine::preset::{Overrides, Speed, VmafOverride};
use cryarchiver_engine::sink::CliSink;
use cryarchiver_engine::text::display_path;
use cryarchiver_engine::toolchain::{Status, ToolConfig, Toolchain};
use cryarchiver_engine::workspace::sweep;
use cryarchiver_tui::recent::{RecentStore, now_unix_ms};
use cryarchiver_tui::{MockSource, ProjectSource};
use tokio::sync::Notify;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::report::{Report, Totals, outcome_line};

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
	#[arg(long, conflicts_with = "inputs", help = "Open the dashboard on demo data")]
	demo: bool,
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
	#[command(about = "Open the archival TUI: a project, or the start menu without one")]
	Tui {
		project: Option<PathBuf>,
		#[arg(long, conflicts_with = "project", help = "Show the dashboard on demo data")]
		demo: bool,
		#[arg(long, value_name = "PATH", help = "ffmpeg binary or the folder containing it")]
		ffmpeg: Option<PathBuf>,
	},
	#[command(about = "Run an archive project without the TUI")]
	Run {
		project: PathBuf,
		#[arg(
			long,
			help = "Confirm that this project deletes each original once its archived copy checks out"
		)]
		delete_originals: bool,
		#[arg(long, value_name = "PATH", help = "ffmpeg binary or the folder containing it")]
		ffmpeg: Option<PathBuf>,
	},
	#[command(subcommand, about = "Create or inspect archive projects")]
	Project(ProjectCommand),
}

#[derive(Subcommand)]
enum ProjectCommand {
	#[command(about = "Create a project that archives SOURCE into DESTINATION")]
	Create {
		source: PathBuf,
		destination: PathBuf,
		#[arg(
			long,
			help = "The project file's name, without .cryarchive [default: the source folder's name]"
		)]
		name: Option<String>,
		#[arg(short, long, value_enum, default_value_t, help = "Speed preset")]
		speed: Speed,
		#[command(flatten)]
		quality: QualityArgs,
		#[arg(long, help = "Keep running and archive new files as they appear")]
		watch: bool,
		#[arg(
			long,
			value_name = "PERCENT",
			value_parser = clap::value_parser!(u8).range(1..=100),
			help = "Run in the background: low priority, one job at a time, this much CPU at most"
		)]
		limit: Option<u8>,
	},
	#[command(about = "Print a project's tables as text, for debugging")]
	Dump { project: PathBuf },
}

#[allow(clippy::struct_excessive_bools)]
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
		value_name = "PATH",
		help = "Output folder, or a file for a single input (its extension picks the format)"
	)]
	output: Option<PathBuf>,
	#[arg(
		short = 'f',
		long,
		value_enum,
		help = "Output format for every file [default: automatic]"
	)]
	format: Option<OutputFormat>,
	#[arg(short, long, value_enum, default_value_t, help = "Speed preset")]
	speed: Speed,
	#[command(flatten)]
	quality: QualityArgs,
	#[arg(long, help = "Keep personal metadata and the source's file timestamps")]
	keep_metadata: bool,
	#[arg(long, help = "Convert photos that have gain or depth maps, dropping the extras")]
	drop_aux: bool,
	#[arg(long, help = "Keep only the main video and the audio")]
	strip: bool,
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

#[derive(Args, Clone, Copy)]
struct QualityArgs {
	#[arg(
		long,
		value_name = "MEAN/FLOOR",
		value_parser = parse_video_vmaf,
		help = "Video VMAF mean and 5th-percentile floor instead of the preset's, e.g. 93/89 (moderate and slow)"
	)]
	video_vmaf: Option<VmafOverride>,
	#[arg(
		long,
		value_name = "N",
		value_parser = parse_image_target,
		help = "Image SSIMULACRA2 target instead of the preset's, 50 to 95"
	)]
	image_target: Option<f64>,
	#[arg(
		long,
		value_name = "N",
		value_parser = parse_audio_kbps,
		help = "Stereo Opus bitrate in kbps instead of the preset's, 64 to 320 in steps of 8; other layouts scale with it"
	)]
	audio_kbps: Option<u32>,
}

impl QualityArgs {
	fn overrides(self) -> Overrides {
		Overrides {
			video: self.video_vmaf,
			image: self.image_target,
			audio_stereo_kbps: self.audio_kbps,
		}
	}
}

fn checked(overrides: Overrides) -> Result<Overrides, String> {
	overrides.validate()?;
	Ok(overrides.normalized())
}

fn number<T: std::str::FromStr>(text: &str) -> Result<T, String> {
	text.trim().parse().map_err(|_| format!("\"{text}\" is not a number"))
}

fn parse_video_vmaf(text: &str) -> Result<VmafOverride, String> {
	let (mean, floor) = text
		.split_once('/')
		.ok_or_else(|| "expected MEAN/FLOOR, such as 93/89".to_owned())?;
	let video = VmafOverride {
		mean: number(mean)?,
		floor: number(floor)?,
	};

	let overrides = checked(Overrides {
		video: Some(video),
		..Overrides::default()
	})?;
	Ok(overrides.video.unwrap_or(video))
}

fn parse_image_target(text: &str) -> Result<f64, String> {
	let target = number(text)?;

	let overrides = checked(Overrides {
		image: Some(target),
		..Overrides::default()
	})?;
	Ok(overrides.image.unwrap_or(target))
}

fn parse_audio_kbps(text: &str) -> Result<u32, String> {
	let kbps = number(text)?;

	let overrides = checked(Overrides {
		audio_stereo_kbps: Some(kbps),
		..Overrides::default()
	})?;
	Ok(overrides.audio_stereo_kbps.unwrap_or(kbps))
}

/// `MODERATE · custom: video VMAF 90/85, image 80, audio 160 kbps`, or `None` without an override that takes effect.
/// The values are the ones the engine uses; a video override stored for FAST is listed last as not used.
fn quality_label(speed: Speed, overrides: &Overrides) -> Option<String> {
	let effective = overrides.effective(speed);
	if effective.is_empty() {
		return None;
	}

	let preset = match speed {
		Speed::Slow => "SLOW",
		Speed::Moderate => "MODERATE",
		Speed::Fast => "FAST",
	};
	let mut values = effective.describe();
	if let Some(video) = overrides.normalized().video.filter(|_| effective.video.is_none()) {
		values.push(format!("video VMAF {}/{} not used at FAST", video.mean, video.floor));
	}

	Some(format!("{preset} · custom: {}", values.join(", ")))
}

struct OutputTarget {
	dir: Option<PathBuf>,
	file: Option<PathBuf>,
	format: Option<OutputFormat>,
}

fn output_target(output: Option<&Path>, format: Option<OutputFormat>, inputs: usize) -> Result<OutputTarget, String> {
	let Some(output) = output else {
		return Ok(OutputTarget {
			dir: None,
			file: None,
			format,
		});
	};

	let Some(named) = OutputFormat::from_path(output).filter(|_| !output.is_dir()) else {
		return Ok(OutputTarget {
			dir: Some(absolute(output.to_path_buf())),
			file: None,
			format,
		});
	};

	if inputs != 1 {
		return Err(format!(
			"-o {} names a file, which works with exactly one input ({inputs} given)",
			display_path(output)
		));
	}
	if let Some(format) = format
		&& format != named
	{
		return Err(format!(
			"-o {} asks for {named} but -f asks for {format}",
			display_path(output)
		));
	}

	Ok(OutputTarget {
		dir: None,
		file: Some(absolute(output.to_path_buf())),
		format: Some(named),
	})
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

#[derive(Debug, PartialEq, Eq)]
enum TuiTarget {
	Menu,
	Demo,
	Project(PathBuf),
}

impl TuiTarget {
	fn new(project: Option<PathBuf>, demo: bool) -> Self {
		match (project, demo) {
			(Some(project), _) => Self::Project(project),
			(None, true) => Self::Demo,
			(None, false) => Self::Menu,
		}
	}
}

enum Launch {
	Tui(TuiTarget, Option<PathBuf>),
	Doctor(Option<PathBuf>),
	Compress(CompressArgs),
	Run {
		project: PathBuf,
		delete_originals: bool,
		ffmpeg: Option<PathBuf>,
	},
	Project(ProjectCommand),
}

fn main() -> ExitCode {
	let code = match launch() {
		Launch::Tui(target, ffmpeg) => return ExitCode::from(open_tui(target, ffmpeg)),
		Launch::Doctor(ffmpeg) => block_on(doctor(ffmpeg)),
		Launch::Compress(args) => block_on(compress(args)),
		Launch::Run {
			project,
			delete_originals,
			ffmpeg,
		} => return ExitCode::from(run_project(&project, delete_originals, ffmpeg)),
		Launch::Project(ProjectCommand::Create {
			source,
			destination,
			name,
			speed,
			quality,
			watch,
			limit,
		}) => projects::create(
			&source,
			&destination,
			name.as_deref(),
			speed,
			quality.overrides(),
			watch,
			limit,
		),
		Launch::Project(ProjectCommand::Dump { project }) => projects::dump(&project),
	};

	pause_if_launched_from_explorer();
	ExitCode::from(code)
}

fn launch() -> Launch {
	launch_from(std::env::args_os().collect())
}

fn launch_from(args: Vec<OsString>) -> Launch {
	if args.len() == 1 {
		return Launch::Tui(TuiTarget::Menu, None);
	}

	let cli = parse(args).unwrap_or_else(|err| err.exit());
	match cli.command {
		Some(Command::Doctor { ffmpeg }) => Launch::Doctor(ffmpeg),
		Some(Command::Tui { project, demo, ffmpeg }) => Launch::Tui(TuiTarget::new(project, demo), ffmpeg),
		Some(Command::Run {
			project,
			delete_originals,
			ffmpeg,
		}) => Launch::Run {
			project,
			delete_originals,
			ffmpeg,
		},
		Some(Command::Project(command)) => Launch::Project(command),
		None if cli.demo => Launch::Tui(TuiTarget::Demo, cli.compress.ffmpeg),
		None if opens_project(&cli.compress.inputs) => Launch::Tui(
			TuiTarget::new(cli.compress.inputs.into_iter().next(), false),
			cli.compress.ffmpeg,
		),
		None => Launch::Compress(cli.compress),
	}
}

/// A top-level `--demo` only goes with the bare launch. Clap would read a subcommand after it as an input file, so
/// that case is refused first, naming both.
fn parse(args: Vec<OsString>) -> Result<Cli, clap::Error> {
	let mut command = Cli::command();
	let first_word = args
		.iter()
		.skip(1)
		.position(|arg| !arg.to_string_lossy().starts_with('-'));
	let subcommand = first_word
		.map(|index| args[index + 1].to_string_lossy().into_owned())
		.filter(|word| command.find_subcommand(word).is_some());

	if let (Some(index), Some(subcommand)) = (first_word, subcommand)
		&& args[1..=index].iter().any(|arg| arg == "--demo")
	{
		return Err(command.error(
			clap::error::ErrorKind::ArgumentConflict,
			format!(
				"--demo can't be used with the {subcommand} command; use \"cryarchiver --demo\" or \"cryarchiver tui --demo\""
			),
		));
	}

	let cli = Cli::try_parse_from(args)?;

	let (speed, quality) = match &cli.command {
		Some(Command::Project(ProjectCommand::Create { speed, quality, .. })) => (*speed, quality),
		Some(_) => return Ok(cli),
		None => (cli.compress.speed, &cli.compress.quality),
	};
	if speed == Speed::Fast && quality.video_vmaf.is_some() {
		return Err(command.error(
			clap::error::ErrorKind::ArgumentConflict,
			"--video-vmaf can't be used with --speed fast: FAST never measures video quality; use moderate or slow",
		));
	}

	Ok(cli)
}

fn block_on<F: Future>(future: F) -> F::Output {
	let runtime = tokio::runtime::Runtime::new().expect("tokio runtime starts");

	let output = runtime.block_on(future);

	// A cancelled image encode keeps running in its blocking thread; waiting for it would delay the exit.
	runtime.shutdown_background();
	output
}

fn opens_project(inputs: &[PathBuf]) -> bool {
	inputs
		.first()
		.is_some_and(|input| input.extension().is_some_and(|ext| ext == "cryarchive"))
}

fn open_tui(target: TuiTarget, ffmpeg: Option<PathBuf>) -> u8 {
	use std::io::IsTerminal;

	let result = match target {
		_ if !io::stdin().is_terminal() || !io::stdout().is_terminal() => Err(cryarchiver_tui::RunError::NotATerminal),
		TuiTarget::Demo => cryarchiver_tui::run(MockSource::default()),
		TuiTarget::Menu => cryarchiver_tui::run_start_menu(RecentStore::locate(), |path| {
			projects::open(path, ffmpeg.clone()).map(ProjectSource::new)
		}),
		TuiTarget::Project(path) => match projects::open(&path, ffmpeg) {
			Ok(project) => {
				let _ = RecentStore::locate().record(&path, now_unix_ms());
				cryarchiver_tui::run(ProjectSource::new(project))
			}
			Err(message) => {
				eprintln!("error: {message}");
				pause_if_launched_from_explorer();
				return 2;
			}
		},
	};

	match result {
		Ok(()) => 0,
		Err(err) => {
			eprintln!("error: {err}");
			pause_if_launched_from_explorer();
			2
		}
	}
}

fn run_project(path: &Path, delete_originals: bool, ffmpeg: Option<PathBuf>) -> u8 {
	let mut project = match projects::open(path, ffmpeg) {
		Ok(project) => project,
		Err(message) => {
			eprintln!("error: {message}");
			return 2;
		}
	};

	let interrupted = Arc::new(AtomicBool::new(false));
	projects::stop_on_ctrl_c(&interrupted);
	headless::run(&mut project, delete_originals, &interrupted)
}

async fn doctor(ffmpeg: Option<PathBuf>) -> u8 {
	let tools = match Toolchain::detect(&ToolConfig { ffmpeg }, &CancellationToken::new()).await {
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
	let target = match output_target(args.output.as_deref(), args.format, inputs.files.len()) {
		Ok(target) => target,
		Err(message) => {
			eprintln!("error: {message}");
			return 2;
		}
	};

	let cancel = CancellationToken::new();
	let tools = match Toolchain::detect(&ToolConfig { ffmpeg: args.ffmpeg }, &cancel).await {
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
		output_dir: target.dir,
		output_file: target.file,
		overwrite: args.overwrite,
	});
	prepare_work_roots(&files, sink.as_ref());
	cancel_on_ctrl_c(&cancel);

	let overrides = args.quality.overrides();
	if let Some(label) = quality_label(args.speed, &overrides)
		&& !args.quiet
	{
		let _ = writeln!(io::stdout(), "{label}");
	}

	let min_savings = f64::from(args.min_savings) / 100.0;
	let jobs = files
		.into_iter()
		.map(|source| Job {
			source,
			speed: args.speed,
			keep_metadata: args.keep_metadata,
			drop_aux: args.drop_aux,
			min_savings,
			overrides,
			options: JobOptions {
				format: target.format,
				strip: args.strip,
				..JobOptions::default()
			},
		})
		.collect();
	let parallel = args.jobs.unwrap_or_else(default_jobs).max(1);
	let engine = Engine::new(tools)
		.with_video_jobs(args.jobs.unwrap_or(1))
		.with_image_threads(image_threads(parallel));
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

		if let Some(existing) = sink.existing_output(&job.source, job.options.format) {
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

fn image_threads(parallel: usize) -> usize {
	std::thread::available_parallelism().map_or(1, |cores| cores.get().div_ceil(parallel))
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

	fn args(args: &[&str]) -> Vec<OsString> {
		std::iter::once("cryarchiver")
			.chain(args.iter().copied())
			.map(OsString::from)
			.collect()
	}

	fn tui_target(args_given: &[&str]) -> Option<TuiTarget> {
		match launch_from(args(args_given)) {
			Launch::Tui(target, _) => Some(target),
			_ => None,
		}
	}

	#[test]
	fn launching_without_a_project_opens_the_start_menu() {
		assert_eq!(tui_target(&[]), Some(TuiTarget::Menu));
		assert_eq!(tui_target(&["tui"]), Some(TuiTarget::Menu));
	}

	#[test]
	fn demo_is_a_flag_on_tui_and_on_the_bare_launch() {
		assert_eq!(tui_target(&["--demo"]), Some(TuiTarget::Demo));
		assert_eq!(tui_target(&["tui", "--demo"]), Some(TuiTarget::Demo));
		assert!(Cli::try_parse_from(args(&["tui", "family.cryarchive", "--demo"])).is_err());
		assert!(Cli::try_parse_from(args(&["--demo", "song.flac"])).is_err());
	}

	#[test]
	fn a_top_level_demo_with_a_subcommand_names_both() {
		for (given, subcommand) in [
			(&["--demo", "doctor"][..], "doctor"),
			(&["--demo", "tui", "family.cryarchive"][..], "tui"),
			(&["--demo", "project", "dump", "family.cryarchive"][..], "project"),
		] {
			let err = parse(args(given))
				.err()
				.unwrap_or_else(|| panic!("{given:?} was accepted"));
			let text = err.to_string();

			assert!(text.contains("--demo"), "{text}");
			assert!(text.contains(subcommand), "{text}");
		}
		assert!(parse(args(&["--demo"])).is_ok());
		assert!(parse(args(&["tui", "--demo"])).is_ok());
	}

	#[test]
	fn a_project_opens_directly() {
		let project = PathBuf::from("family.cryarchive");

		assert_eq!(
			tui_target(&["tui", "family.cryarchive"]),
			Some(TuiTarget::Project(project.clone()))
		);
		assert_eq!(tui_target(&["family.cryarchive"]), Some(TuiTarget::Project(project)));
		assert_eq!(tui_target(&["song.flac"]), None);
	}

	fn compress_overrides(given: &[&str]) -> Overrides {
		match launch_from(args(given)) {
			Launch::Compress(compress) => compress.quality.overrides(),
			_ => panic!("{given:?} is not a compress run"),
		}
	}

	fn refusal(given: &[&str]) -> clap::Error {
		parse(args(given))
			.err()
			.unwrap_or_else(|| panic!("{given:?} was accepted"))
	}

	#[test]
	fn quality_flags_become_normalized_overrides() {
		assert_eq!(compress_overrides(&["a.flac"]), Overrides::default());
		assert_eq!(
			compress_overrides(&[
				"a.flac",
				"--video-vmaf",
				"90/85",
				"--image-target",
				"80",
				"--audio-kbps",
				"160"
			]),
			Overrides {
				video: Some(VmafOverride {
					mean: 90.0,
					floor: 85.0
				}),
				image: Some(80.0),
				audio_stereo_kbps: Some(160),
			}
		);
		assert_eq!(
			compress_overrides(&["a.flac", "--video-vmaf", "90.04/85.06", "--image-target", "62.46"]),
			Overrides {
				video: Some(VmafOverride {
					mean: 90.0,
					floor: 85.1
				}),
				image: Some(62.5),
				audio_stereo_kbps: None,
			}
		);

		let Launch::Project(ProjectCommand::Create { quality, .. }) =
			launch_from(args(&["project", "create", "src", "dst", "--image-target", "80"]))
		else {
			panic!("project create was not parsed");
		};
		assert_eq!(quality.overrides().image, Some(80.0));
	}

	#[test]
	fn quality_flags_out_of_range_exit_with_2_and_name_the_range() {
		for (given, range) in [
			(&["a.flac", "--video-vmaf", "79.9/70"][..], "between 80 and 99"),
			(&["a.flac", "--video-vmaf", "99.1/90"][..], "between 80 and 99"),
			(&["a.flac", "--video-vmaf", "90/69"][..], "between 70 and the mean (90)"),
			(&["a.flac", "--video-vmaf", "90/91"][..], "between 70 and the mean (90)"),
			(&["a.flac", "--video-vmaf", "nan/80"][..], "between 80 and 99"),
			(&["a.flac", "--video-vmaf", "90"][..], "MEAN/FLOOR"),
			(&["a.flac", "--image-target", "49"][..], "between 50 and 95"),
			(&["a.flac", "--image-target", "96"][..], "between 50 and 95"),
			(&["a.flac", "--image-target", "high"][..], "not a number"),
			(
				&["a.flac", "--audio-kbps", "56"][..],
				"between 64 and 320 kbps, in steps of 8",
			),
			(
				&["a.flac", "--audio-kbps", "328"][..],
				"between 64 and 320 kbps, in steps of 8",
			),
			(
				&["a.flac", "--audio-kbps", "100"][..],
				"between 64 and 320 kbps, in steps of 8",
			),
			(
				&["project", "create", "src", "dst", "--audio-kbps", "400"][..],
				"between 64 and 320 kbps",
			),
		] {
			let err = refusal(given);

			assert_eq!(err.exit_code(), 2, "{given:?}");
			assert!(err.to_string().contains(range), "{given:?}: {err}");
		}
	}

	#[test]
	fn a_video_target_at_fast_is_refused() {
		for given in [
			&["a.mp4", "--speed", "fast", "--video-vmaf", "93/89"][..],
			&["project", "create", "src", "dst", "-s", "fast", "--video-vmaf", "93/89"][..],
		] {
			let err = refusal(given);

			assert_eq!(err.exit_code(), 2, "{given:?}");
			assert!(
				err.to_string()
					.contains("FAST never measures video quality; use moderate or slow"),
				"{given:?}: {err}"
			);
		}

		assert!(
			parse(args(&[
				"a.mp4",
				"--speed",
				"fast",
				"--image-target",
				"80",
				"--audio-kbps",
				"96"
			]))
			.is_ok()
		);
		assert!(parse(args(&["a.mp4", "--speed", "slow", "--video-vmaf", "93/89"])).is_ok());
	}

	#[test]
	fn a_custom_profile_is_labelled_with_its_preset_and_values() {
		let custom = Overrides {
			video: Some(VmafOverride {
				mean: 90.0,
				floor: 85.0,
			}),
			image: Some(80.0),
			audio_stereo_kbps: Some(160),
		};

		assert_eq!(quality_label(Speed::Moderate, &Overrides::default()), None);
		assert_eq!(
			quality_label(Speed::Moderate, &custom).as_deref(),
			Some("MODERATE · custom: video VMAF 90/85, image 80, audio 160 kbps")
		);
		assert_eq!(
			quality_label(
				Speed::Slow,
				&Overrides {
					image: Some(62.5),
					..Overrides::default()
				}
			)
			.as_deref(),
			Some("SLOW · custom: image 62.5")
		);
	}

	#[test]
	fn the_label_counts_only_overrides_that_take_effect() {
		let video = Overrides {
			video: Some(VmafOverride {
				mean: 90.0,
				floor: 85.0,
			}),
			..Overrides::default()
		};
		let video_and_image = Overrides {
			image: Some(80.0),
			..video
		};

		assert_eq!(quality_label(Speed::Fast, &video), None);
		assert_eq!(
			quality_label(Speed::Fast, &video_and_image).as_deref(),
			Some("FAST · custom: image 80, video VMAF 90/85 not used at FAST")
		);
		assert_eq!(
			quality_label(Speed::Moderate, &video).as_deref(),
			Some("MODERATE · custom: video VMAF 90/85")
		);
	}

	#[test]
	fn the_label_shows_the_values_the_engine_uses() {
		let stored = Overrides {
			image: Some(200.0),
			audio_stereo_kbps: Some(99),
			..Overrides::default()
		};

		assert_eq!(
			quality_label(Speed::Moderate, &stored).as_deref(),
			Some("MODERATE · custom: image 95, audio 96 kbps")
		);
		assert_eq!(
			quality_label(
				Speed::Moderate,
				&Overrides {
					image: Some(f64::NAN),
					..Overrides::default()
				}
			),
			None
		);
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
