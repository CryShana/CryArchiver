pub mod analyze;
pub mod colour;
pub mod encode;
pub mod plan;
pub mod streams;

use tokio_util::sync::CancellationToken;

use crate::outcome::{ErrorKind, JobError};
use crate::process::{self, Progress, StdoutMode, ToolCommand, ToolOutput};

pub(crate) async fn run_ffmpeg(
	cmd: &ToolCommand,
	on_progress: impl FnMut(&Progress),
	cancel: &CancellationToken,
) -> Result<ToolOutput, JobError> {
	let out = process::run(cmd, StdoutMode::Progress, on_progress, cancel).await?;
	if out.completed() {
		return Ok(out);
	}

	let reason = process::failure_reason(&out.stderr_tail);
	Err(JobError::with_log(ErrorKind::EncodeFailed(reason), out.stderr_tail))
}
