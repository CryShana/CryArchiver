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
}

impl ToolCommand {
	pub fn new(program: &Path, stall_timeout: Duration) -> Self {
		Self {
			program: program.to_path_buf(),
			args: Vec::new(),
			stall_timeout,
			counted: &[],
		}
	}

	pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
		self.args.push(arg.into());
		self
	}

	pub fn args<S: Into<OsString>>(&mut self, args: impl IntoIterator<Item = S>) -> &mut Self {
		self.args.extend(args.into_iter().map(Into::into));
		self
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
}

pub async fn run(
	cmd: &ToolCommand,
	mode: StdoutMode,
	mut on_progress: impl FnMut(&Progress),
	cancel: &CancellationToken,
) -> Result<ToolOutput, RunError> {
	if cancel.is_cancelled() {
		return Err(RunError::Cancelled);
	}

	let mut child = spawn(cmd, mode).map_err(RunError::Spawn)?;
	let mut stdout = child.stdout().take().expect("stdout is piped");
	let stderr = tokio::spawn(collect_stderr(
		child.stderr().take().expect("stderr is piped"),
		cmd.counted,
	));

	let mut captured = Vec::new();
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
				captured.extend_from_slice(&buf[..n]);
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

	// After a cancel the exit status proves nothing about the file: the same interruption may have ended the tool.
	if cancel.is_cancelled() {
		return Err(RunError::Cancelled);
	}

	let (stderr_tail, counted_lines) = stderr.await.unwrap_or_default();
	Ok(ToolOutput {
		status,
		stdout: captured,
		stderr_tail,
		counted_lines,
		last_progress,
	})
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

		#[cfg(target_os = "linux")]
		die_with_parent(command);
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
fn die_with_parent(command: &mut tokio::process::Command) {
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
