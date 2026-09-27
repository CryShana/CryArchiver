use std::path::Path;
use std::time::{Duration, Instant};

use cryarchiver_engine::process::{RunError, StdoutMode, ToolCommand, run};
use tokio_util::sync::CancellationToken;

const PATIENT: Duration = Duration::from_secs(60);

fn tool(program: &str, args: &[&str], stall_timeout: Duration) -> ToolCommand {
	let mut cmd = ToolCommand::new(Path::new(program), stall_timeout);
	cmd.args(args);
	cmd
}

#[tokio::test]
async fn captured_output_past_the_limit_stops_the_tool() {
	let mut cmd = tool(
		"ffmpeg",
		&[
			"-hide_banner",
			"-nostdin",
			"-v",
			"error",
			"-f",
			"lavfi",
			"-i",
			"color=size=64x64:duration=1",
			"-f",
			"rawvideo",
			"-",
		],
		PATIENT,
	);
	cmd.capture_limit = Some(1000);

	let result = run(&cmd, StdoutMode::Capture, |_| {}, &CancellationToken::new()).await;

	assert!(matches!(result, Err(RunError::CaptureLimit(1000))), "{result:?}");
}

#[tokio::test]
async fn reports_progress_until_end() {
	let cmd = tool(
		"ffmpeg",
		&[
			"-hide_banner",
			"-nostdin",
			"-f",
			"lavfi",
			"-i",
			"sine=duration=2",
			"-f",
			"null",
			"-",
		],
		PATIENT,
	);
	let mut updates = 0;

	let out = run(&cmd, StdoutMode::Progress, |_| updates += 1, &CancellationToken::new())
		.await
		.unwrap();

	assert!(out.completed());
	assert!(updates >= 1);
	assert!(out.last_progress.unwrap().out_time.unwrap() >= Duration::from_millis(1900));
}

#[tokio::test]
async fn captures_stdout_for_json_tools() {
	let cmd = tool(
		"ffprobe",
		&[
			"-v",
			"error",
			"-f",
			"lavfi",
			"-i",
			"sine=duration=1",
			"-show_entries",
			"stream=codec_name",
			"-of",
			"json",
		],
		PATIENT,
	);

	let out = run(&cmd, StdoutMode::Capture, |_| {}, &CancellationToken::new())
		.await
		.unwrap();

	assert!(out.status.success());
	assert!(String::from_utf8_lossy(&out.stdout).contains("pcm_s16le"));
}

#[tokio::test]
async fn keeps_the_stderr_tail_on_failure() {
	let cmd = tool(
		"ffmpeg",
		&[
			"-hide_banner",
			"-nostdin",
			"-i",
			"does-not-exist.wav",
			"-f",
			"null",
			"-",
		],
		PATIENT,
	);

	let out = run(&cmd, StdoutMode::Progress, |_| {}, &CancellationToken::new())
		.await
		.unwrap();

	assert!(!out.status.success());
	assert!(out.stderr_tail.iter().any(|line| line.contains("does-not-exist.wav")));
}

#[tokio::test]
async fn cancel_kills_the_process_quickly() {
	let cmd = tool(
		"ffmpeg",
		&[
			"-hide_banner",
			"-nostdin",
			"-re",
			"-f",
			"lavfi",
			"-i",
			"sine=duration=60",
			"-f",
			"null",
			"-",
		],
		PATIENT,
	);
	let cancel = CancellationToken::new();
	let trigger = cancel.clone();
	tokio::spawn(async move {
		tokio::time::sleep(Duration::from_millis(500)).await;
		trigger.cancel();
	});
	let started = Instant::now();

	let result = run(&cmd, StdoutMode::Progress, |_| {}, &cancel).await;

	assert!(matches!(result, Err(RunError::Cancelled)));
	assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn stall_timeout_fires_without_progress() {
	let cmd = tool(
		"ffmpeg",
		&[
			"-hide_banner",
			"-nostdin",
			"-re",
			"-f",
			"lavfi",
			"-i",
			"sine=duration=60",
			"-stats_period",
			"30",
			"-f",
			"null",
			"-",
		],
		Duration::from_secs(1),
	);

	let result = run(&cmd, StdoutMode::Progress, |_| {}, &CancellationToken::new()).await;

	assert!(matches!(result, Err(RunError::Stalled(_))));
}

#[tokio::test]
async fn already_cancelled_never_spawns() {
	let cancel = CancellationToken::new();
	cancel.cancel();

	let result = run(
		&tool("no-such-tool", &[], PATIENT),
		StdoutMode::Capture,
		|_| {},
		&cancel,
	)
	.await;

	assert!(matches!(result, Err(RunError::Cancelled)));
}

#[tokio::test]
async fn counts_matching_stderr_lines_beyond_the_tail() {
	let mut cmd = tool(
		"ffmpeg",
		&[
			"-hide_banner",
			"-nostdin",
			"-f",
			"lavfi",
			"-i",
			"sine=duration=20",
			"-af",
			"ashowinfo",
			"-f",
			"null",
			"-",
		],
		PATIENT,
	);
	cmd.count_lines_containing(&["Parsed_ashowinfo_0"]);

	let out = run(&cmd, StdoutMode::Progress, |_| {}, &CancellationToken::new())
		.await
		.unwrap();

	assert!(out.completed());
	assert!(out.counted_lines >= 800, "{}", out.counted_lines);
	assert!(out.stderr_tail.len() <= 500);
}

#[tokio::test]
async fn counts_nothing_without_patterns() {
	let cmd = tool(
		"ffmpeg",
		&[
			"-hide_banner",
			"-nostdin",
			"-f",
			"lavfi",
			"-i",
			"sine=duration=1",
			"-af",
			"ashowinfo",
			"-f",
			"null",
			"-",
		],
		PATIENT,
	);

	let out = run(&cmd, StdoutMode::Progress, |_| {}, &CancellationToken::new())
		.await
		.unwrap();

	assert_eq!(out.counted_lines, 0);
}
