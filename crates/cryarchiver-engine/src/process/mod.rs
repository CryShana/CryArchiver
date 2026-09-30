pub mod limits;
mod lines;
mod progress;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::io::AsyncReadExt;
use tokio::process::ChildStderr;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
#[cfg(windows)]
use windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

#[cfg(target_os = "linux")]
use limits::Priority;
use lines::{LineSplitter, RingBuffer};
pub use progress::Progress;
use progress::{HighWater, ProgressParser};

pub const STALL_TIMEOUT: Duration = Duration::from_secs(600);

const STDERR_LINES: usize = 500;
const SILENT_BYTES_PER_SECOND: u64 = 20_000_000;

#[derive(Debug, Clone)]
pub struct ToolCommand {
	pub program: PathBuf,
	pub args: Vec<OsString>,
	pub stall_timeout: Duration,
	pub counted: &'static [&'static str],
	pub capture_limit: Option<usize>,
	pub current_dir: Option<PathBuf>,
}

impl ToolCommand {
	pub fn new(program: &Path, stall_timeout: Duration) -> Self {
		Self {
			program: program.to_path_buf(),
			args: Vec::new(),
			stall_timeout,
			counted: &[],
			capture_limit: None,
			current_dir: None,
		}
	}

	pub fn in_dir(&mut self, dir: &Path) -> &mut Self {
		self.current_dir = Some(dir.to_path_buf());
		self
	}

	pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
		self.args.push(arg.into());
		self
	}

	pub fn args<S: Into<OsString>>(&mut self, args: impl IntoIterator<Item = S>) -> &mut Self {
		self.args.extend(args.into_iter().map(Into::into));
		self
	}

	/// Input options: they apply to the next `-i`.
	pub fn seek(&mut self, start_s: f64, length_s: f64) -> &mut Self {
		self.arg("-ss")
			.arg(format!("{start_s:.3}"))
			.arg("-t")
			.arg(format!("{length_s:.3}"))
	}

	pub fn count_lines_containing(&mut self, patterns: &'static [&'static str]) -> &mut Self {
		self.counted = patterns;
		self
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdoutMode {
	Capture,
	Progress,
}

#[derive(Debug)]
pub struct ToolOutput {
	pub status: ExitStatus,
	pub stdout: Vec<u8>,
	pub stderr_tail: Vec<String>,
	pub counted_lines: usize,
	pub last_progress: Option<Progress>,
}

impl ToolOutput {
	pub fn completed(&self) -> bool {
		self.status.success() && self.last_progress.as_ref().is_some_and(|progress| progress.done)
	}
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
	#[error("could not start the tool: {0}")]
	Spawn(io::Error),
	#[error("i/o error while running the tool: {0}")]
	Io(io::Error),
	#[error("no progress for {0:?}")]
	Stalled(Duration),
	#[error("cancelled")]
	Cancelled,
	#[error("the tool wrote more than {0} bytes")]
	CaptureLimit(usize),
	#[error("the tool crashed: {description}")]
	Crashed {
		description: String,
		status: ExitStatus,
		stderr_tail: Vec<String>,
	},
}

type ChunkSink<'a> = dyn FnMut(&[u8]) + Send + 'a;

pub async fn run(
	cmd: &ToolCommand,
	mode: StdoutMode,
	on_progress: impl FnMut(&Progress),
	cancel: &CancellationToken,
) -> Result<ToolOutput, RunError> {
	drive(cmd, mode, on_progress, None, cancel).await
}

/// `run` in `Capture` mode, handing stdout to `on_chunk` as it arrives instead of keeping it. `capture_limit` caps
/// the bytes streamed.
pub async fn run_streamed(
	cmd: &ToolCommand,
	mut on_chunk: impl FnMut(&[u8]) + Send,
	cancel: &CancellationToken,
) -> Result<ToolOutput, RunError> {
	drive(cmd, StdoutMode::Capture, |_| {}, Some(&mut on_chunk), cancel).await
}

async fn drive(
	cmd: &ToolCommand,
	mode: StdoutMode,
	mut on_progress: impl FnMut(&Progress),
	mut on_chunk: Option<&mut ChunkSink<'_>>,
	cancel: &CancellationToken,
) -> Result<ToolOutput, RunError> {
	if cancel.is_cancelled() {
		return Err(RunError::Cancelled);
	}

	let mut child = spawn(cmd, mode).map_err(RunError::Spawn)?;
	let tracked = limits::track(child.id());
	let mut stdout = child.stdout().take().expect("stdout is piped");
	let stderr = tokio::spawn(collect_stderr(
		child.stderr().take().expect("stderr is piped"),
		cmd.counted,
	));

	let mut captured = Vec::new();
	let mut received = 0;
	let mut splitter = LineSplitter::default();
	let mut parser = ProgressParser::default();
	let mut high_water = HighWater::default();
	let mut last_progress: Option<Progress> = None;
	let mut last_advance = Instant::now();
	let mut buf = [0u8; 8192];

	let status = loop {
		let read = tokio::select! {
			() = cancel.cancelled() => return Err(stop(child.as_mut(), RunError::Cancelled).await),
			read = tokio::time::timeout_at(last_advance + cmd.stall_timeout, stdout.read(&mut buf)) => read,
		};

		match read {
			Err(_) => return Err(stop(child.as_mut(), RunError::Stalled(cmd.stall_timeout)).await),
			Ok(Err(err)) => return Err(stop(child.as_mut(), RunError::Io(err)).await),
			Ok(Ok(0)) => break child.wait().await.map_err(RunError::Io)?,
			Ok(Ok(n)) if mode == StdoutMode::Capture => {
				if let Some(limit) = cmd.capture_limit
					&& received + n > limit
				{
					return Err(stop(child.as_mut(), RunError::CaptureLimit(limit)).await);
				}

				received += n;
				match on_chunk.as_mut() {
					Some(on_chunk) => on_chunk(&buf[..n]),
					None => captured.extend_from_slice(&buf[..n]),
				}
				last_advance = Instant::now();
			}
			Ok(Ok(n)) => {
				for line in splitter.push(&buf[..n]) {
					let Some(progress) = parser.line(&line) else { continue };

					if high_water.advance(&progress) {
						last_advance = Instant::now();
					}

					on_progress(&progress);
					last_progress = Some(progress);
				}
			}
		}
	};
	drop(tracked);

	// After a cancel the exit status proves nothing about the file: the same interruption may have ended the tool.
	if cancel.is_cancelled() {
		return Err(RunError::Cancelled);
	}

	let (stderr_tail, counted_lines) = stderr.await.unwrap_or_default();

	if let Some(description) = crash_description(status) {
		return Err(RunError::Crashed {
			description,
			status,
			stderr_tail,
		});
	}

	Ok(ToolOutput {
		status,
		stdout: captured,
		stderr_tail,
		counted_lines,
		last_progress,
	})
}

/// A system exception code: NTSTATUS error severity with the customer and reserved bits clear. ffmpeg exits with
/// negative AVERROR values, and the errno ones (-2 is 0xFFFFFFFE) have those bits set, so they stay plain failures.
/// Ctrl+C's own exit code is an interruption, not a crash. The one AVERROR that counts is ENOMEM (-12): a failed
/// allocation, which fewer threads may avoid.
#[cfg(windows)]
fn crash_description(status: ExitStatus) -> Option<String> {
	const TOP_BITS: u32 = 0xF000_0000;
	const SYSTEM_ERROR: u32 = 0xC000_0000;
	const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;
	const ENOMEM_EXIT: u32 = 0xFFFF_FFF4;

	#[allow(clippy::cast_sign_loss)]
	let code = status.code()? as u32;
	if code == ENOMEM_EXIT {
		return Some("out of memory (exit code -12)".to_owned());
	}

	if code & TOP_BITS != SYSTEM_ERROR || code == STATUS_CONTROL_C_EXIT {
		return None;
	}

	let name = match code {
		0xC000_0005 => " (access violation)",
		0xC000_0017 | 0xC000_012D => " (out of memory)",
		0xC000_001D => " (illegal instruction)",
		0xC000_00FD => " (stack overflow)",
		0xC000_0409 => " (aborted)",
		_ => "",
	};
	Some(format!("exception 0x{code:08X}{name}"))
}

/// SIGINT, SIGTERM and SIGHUP are someone stopping the tool, so only the signals of a failing process count. Exit
/// code 244 is ffmpeg's AVERROR(ENOMEM) as a byte: a failed allocation, which fewer threads may avoid.
#[cfg(unix)]
fn crash_description(status: ExitStatus) -> Option<String> {
	use std::os::unix::process::ExitStatusExt;

	const ENOMEM_EXIT: i32 = 244;

	if status.code() == Some(ENOMEM_EXIT) {
		return Some("out of memory (exit code 244)".to_owned());
	}

	let signal = status.signal()?;
	let name = match signal {
		libc::SIGSEGV => "segmentation fault",
		libc::SIGBUS => "bus error",
		libc::SIGILL => "illegal instruction",
		libc::SIGFPE => "arithmetic error",
		libc::SIGABRT => "aborted",
		libc::SIGKILL => "killed, possibly for running out of memory",
		_ => return None,
	};
	Some(format!("signal {signal}, {name}"))
}

pub fn scaled_timeout(bytes: u64) -> Duration {
	STALL_TIMEOUT + Duration::from_secs(bytes / SILENT_BYTES_PER_SECOND)
}

pub fn tool_path(path: &Path) -> PathBuf {
	std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn spawn(cmd: &ToolCommand, mode: StdoutMode) -> io::Result<Box<dyn ChildWrapper>> {
	let mut command = CommandWrap::with_new(&cmd.program, |command| {
		if mode == StdoutMode::Progress {
			command.args(["-progress", "pipe:1", "-nostats"]);
		}
		command
			.args(&cmd.args)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped());
		if let Some(dir) = &cmd.current_dir {
			command.current_dir(dir);
		}

		#[cfg(target_os = "linux")]
		die_with_parent(command, limits::current().priority);
	});
	command.wrap(KillOnDrop);

	// The console sends Ctrl+C to every process attached to it; in a new process group the tools ignore it.
	#[cfg(windows)]
	command
		.wrap(process_wrap::tokio::CreationFlags(CREATE_NEW_PROCESS_GROUP))
		.wrap(process_wrap::tokio::JobObject);

	#[cfg(unix)]
	command.wrap(process_wrap::tokio::ProcessGroup::leader());

	command.spawn()
}

// PR_SET_PDEATHSIG fires when the spawning *thread* exits, not the process: spawn children from the runtime's
// long-lived worker threads, never from `spawn_blocking`.
#[cfg(target_os = "linux")]
fn die_with_parent(command: &mut tokio::process::Command, priority: Priority) {
	// SAFETY: getpid has no preconditions.
	let parent = unsafe { libc::getpid() };

	// SAFETY: prctl, getppid and _exit are async-signal-safe and only affect the forked child.
	unsafe {
		command.pre_exec(move || {
			libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);

			// If the parent died before prctl ran, the death signal will never come.
			if libc::getppid() != parent {
				libc::_exit(1);
			}

			if priority != Priority::Normal {
				limits::lower_this_process(priority);
			}
			Ok(())
		});
	}
}

async fn collect_stderr(mut stderr: ChildStderr, counted: &'static [&'static str]) -> (Vec<String>, usize) {
	let mut splitter = LineSplitter::default();
	let mut log = StderrLog {
		ring: RingBuffer::new(STDERR_LINES),
		counted,
		matches: 0,
	};
	let mut buf = [0u8; 8192];

	while let Ok(n) = stderr.read(&mut buf).await {
		if n == 0 {
			break;
		}
		splitter.push(&buf[..n]).into_iter().for_each(|line| log.push(line));
	}

	if let Some(line) = splitter.finish() {
		log.push(line);
	}
	(log.ring.into_vec(), log.matches)
}

struct StderrLog {
	ring: RingBuffer,
	counted: &'static [&'static str],
	matches: usize,
}

impl StderrLog {
	fn push(&mut self, line: String) {
		if line.starts_with('[') && self.counted.iter().any(|pattern| line.contains(pattern)) {
			self.matches += 1;
		}
		self.ring.push(line);
	}
}

async fn stop(child: &mut dyn ChildWrapper, error: RunError) -> RunError {
	let _ = child.start_kill();
	let _ = child.wait().await;
	error
}

const GENERIC_TRAILERS: &[&str] = &["Conversion failed!", "Error closing file", "Exiting normally"];

pub fn failure_reason(stderr: &[String]) -> String {
	stderr
		.iter()
		.rev()
		.find(|line| !line.trim().is_empty() && !GENERIC_TRAILERS.iter().any(|generic| line.contains(generic)))
		.cloned()
		.unwrap_or_else(|| "ffmpeg exited with an error".into())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tool_paths_are_absolute() {
		let relative = Path::new("music").join("-intro.flac");
		let absolute = std::env::temp_dir().join("a.flac");

		let resolved = tool_path(&relative);

		assert!(resolved.is_absolute(), "{}", resolved.display());
		assert!(resolved.ends_with(&relative), "{}", resolved.display());
		assert_eq!(tool_path(&absolute), absolute);
	}

	#[test]
	fn silent_steps_get_a_second_per_20_mb() {
		assert_eq!(scaled_timeout(0), STALL_TIMEOUT);
		assert_eq!(scaled_timeout(19_999_999), STALL_TIMEOUT);
		assert_eq!(
			scaled_timeout(50_000_000_000),
			STALL_TIMEOUT + Duration::from_secs(2500)
		);
	}

	#[test]
	fn failure_reason_skips_generic_trailers() {
		let stderr = [
			"[dec:flac @ 0x1] Decoding error: Invalid data found when processing input",
			"[out#0/opus @ 0x2] Error closing file: Invalid argument",
			"Conversion failed!",
			"",
		]
		.map(str::to_owned);

		assert_eq!(
			failure_reason(&stderr),
			"[dec:flac @ 0x1] Decoding error: Invalid data found when processing input"
		);
		assert_eq!(failure_reason(&[]), "ffmpeg exited with an error");
		assert_eq!(
			failure_reason(&["Conversion failed!".to_owned()]),
			"ffmpeg exited with an error"
		);
	}
}
