use std::path::Path;
use std::time::{Duration, Instant};

use cryarchiver_engine::outcome::{ErrorKind, JobError};
use cryarchiver_engine::process::{RunError, StdoutMode, ToolCommand, run, run_streamed};
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
async fn streamed_output_is_handed_over_not_kept_and_the_limit_counts_it() {
	let rawvideo = |limit: Option<usize>| {
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
				"color=size=64x64:rate=25:duration=1,format=gray",
				"-f",
				"rawvideo",
				"-",
			],
			PATIENT,
		);
		cmd.capture_limit = limit;
		cmd
	};
	let mut streamed = 0;

	let out = run_streamed(
		&rawvideo(None),
		|chunk| streamed += chunk.len(),
		&CancellationToken::new(),
	)
	.await
	.unwrap();
	let limited = run_streamed(&rawvideo(Some(1000)), |_| {}, &CancellationToken::new()).await;

	assert_eq!(streamed, 25 * 64 * 64);
	assert!(out.stdout.is_empty());
	assert!(matches!(limited, Err(RunError::CaptureLimit(1000))), "{limited:?}");
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
async fn tools_run_in_their_working_directory() {
	let dir = tempfile::tempdir().unwrap();
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
			"sine=duration=0.1",
			"tone.wav",
		],
		PATIENT,
	);
	cmd.in_dir(dir.path());

	let out = run(&cmd, StdoutMode::Progress, |_| {}, &CancellationToken::new())
		.await
		.unwrap();

	assert!(out.completed());
	assert!(dir.path().join("tone.wav").is_file());
}

#[cfg(windows)]
fn shell(script: &[&str]) -> ToolCommand {
	let args: Vec<&str> = std::iter::once("/c").chain(script.iter().copied()).collect();
	tool("cmd", &args, PATIENT)
}

#[cfg(unix)]
fn shell(script: &[&str]) -> ToolCommand {
	tool("sh", &["-c", &script.join(" ")], PATIENT)
}

#[cfg(windows)]
const CRASHES: &[(&[&str], &str)] = &[
	(
		&["echo", "boom", "1>&2", "&", "exit", "-1073741819"],
		"0xC0000005 (access violation)",
	),
	(
		&["echo", "boom", "1>&2", "&", "exit", "-1073741801"],
		"0xC0000017 (out of memory)",
	),
	(
		&["echo", "boom", "1>&2", "&", "exit", "-1073740791"],
		"0xC0000409 (aborted)",
	),
	(
		&["echo", "boom", "1>&2", "&", "exit", "-12"],
		"out of memory (exit code -12)",
	),
];

#[cfg(unix)]
const CRASHES: &[(&[&str], &str)] = &[
	(&["echo", "boom", ">&2;", "kill", "-SEGV", "$$"], "segmentation fault"),
	(&["echo", "boom", ">&2;", "kill", "-ABRT", "$$"], "aborted"),
	(&["echo", "boom", ">&2;", "kill", "-KILL", "$$"], "out of memory"),
	(
		&["echo", "boom", ">&2;", "exit", "244"],
		"out of memory (exit code 244)",
	),
];

#[cfg(windows)]
const ORDINARY_FAILURES: &[&[&str]] = &[
	&["exit", "1"],
	&["exit", "255"],
	&["exit", "69"],
	&["exit", "244"],
	&["exit", "-2"],
	&["exit", "-11"],
	&["exit", "-13"],
	&["exit", "-22"],
	&["exit", "-1094995529"],
	&["exit", "-1073741510"],
];

#[cfg(unix)]
const ORDINARY_FAILURES: &[&[&str]] = &[
	&["exit", "1"],
	&["exit", "255"],
	&["exit", "69"],
	&["exit", "254"],
	&["exit", "243"],
	&["exit", "245"],
	&["kill", "-INT", "$$"],
	&["kill", "-TERM", "$$"],
	&["kill", "-HUP", "$$"],
];

#[tokio::test]
async fn a_tool_that_crashes_is_reported_as_crashed_with_its_stderr() {
	for (script, named) in CRASHES {
		let result = run(&shell(script), StdoutMode::Capture, |_| {}, &CancellationToken::new()).await;

		let Err(err @ RunError::Crashed { .. }) = result else {
			panic!("{script:?}: {result:?}");
		};
		let job = JobError::from(err);
		let ErrorKind::ToolCrashed(description) = &job.kind else {
			panic!("{script:?}: {job:?}");
		};
		assert!(description.contains(named), "{script:?}: {description}");
		assert!(
			job.log_tail.iter().any(|line| line.contains("boom")),
			"{script:?}: {job:?}"
		);
	}
}

#[tokio::test]
async fn ordinary_failures_and_interruptions_stay_an_exit_status() {
	for script in ORDINARY_FAILURES {
		let out = run(&shell(script), StdoutMode::Capture, |_| {}, &CancellationToken::new())
			.await
			.unwrap_or_else(|err| panic!("{script:?}: {err:?}"));

		assert!(!out.status.success(), "{script:?}");
	}
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
