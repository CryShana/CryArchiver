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

use lines::{LineSplitter, RingBuffer};
pub use progress::Progress;
use progress::{HighWater, ProgressParser};

pub const STALL_TIMEOUT: Duration = Duration::from_secs(600);

const STDERR_LINES: usize = 500;

#[derive(Debug, Clone)]
pub struct ToolCommand {
	pub program: PathBuf,
	pub args: Vec<OsString>,
	pub stall_timeout: Duration,
}

impl ToolCommand {
	pub fn new(program: &Path, stall_timeout: Duration) -> Self {
		Self {
			program: program.to_path_buf(),
			args: Vec::new(),
			stall_timeout,
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
	let stderr = tokio::spawn(collect_stderr(child.stderr().take().expect("stderr is piped")));

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

	let stderr_tail = stderr.await.unwrap_or_default();
	Ok(ToolOutput {
		status,
		stdout: captured,
		stderr_tail,
		last_progress,
	})
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

	#[cfg(windows)]
	command.wrap(process_wrap::tokio::JobObject);

	#[cfg(unix)]
	command.wrap(process_wrap::tokio::ProcessGroup::leader());

	command.spawn()
}

// PR_SET_PDEATHSIG fires when the spawning *thread* exits, not the process: spawn children from the runtime's
// long-lived worker threads, never from `spawn_blocking`.
#[cfg(target_os = "linux")]
fn die_with_parent(command: &mut tokio::process::Command) {
	// SAFETY: prctl is async-signal-safe and only affects the forked child.
	unsafe {
		command.pre_exec(|| {
			libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
			Ok(())
		});
	}
}

async fn collect_stderr(mut stderr: ChildStderr) -> Vec<String> {
	let mut splitter = LineSplitter::default();
	let mut ring = RingBuffer::new(STDERR_LINES);
	let mut buf = [0u8; 8192];

	while let Ok(n) = stderr.read(&mut buf).await {
		if n == 0 {
			break;
		}
		splitter.push(&buf[..n]).into_iter().for_each(|line| ring.push(line));
	}

	if let Some(line) = splitter.finish() {
		ring.push(line);
	}
	ring.into_vec()
}

async fn stop(child: &mut dyn ChildWrapper, error: RunError) -> RunError {
	let _ = child.start_kill();
	let _ = child.wait().await;
	error
}
