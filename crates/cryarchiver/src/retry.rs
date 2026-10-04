use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::engine::Job;
use cryarchiver_engine::outcome::{ErrorClass, ErrorKind, JobError, JobOutcome, threads_note};
use indicatif::DecimalBytes;
use tokio_util::sync::CancellationToken;

pub const MAX_ATTEMPTS: u32 = 3;

const DELAY: Duration = Duration::from_secs(5);
const STABLE_POLL: Duration = Duration::from_secs(1);
const STABLE_FOR: Duration = Duration::from_secs(5);
const STABLE_LIMIT: Duration = Duration::from_secs(300);
const SPACE_POLL: Duration = Duration::from_secs(60);
const SPACE_LIMIT: Duration = Duration::from_mins(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
	Delay(Duration),
	Stable,
	Space { need: u64 },
}

pub fn next_wait(kind: &ErrorKind, attempt: u32) -> Option<Wait> {
	if attempt >= MAX_ATTEMPTS || kind.class() != ErrorClass::AutoRetry {
		return None;
	}

	Some(match kind {
		ErrorKind::SourceChanged => Wait::Stable,
		ErrorKind::InsufficientSpace { need, .. } => Wait::Space { need: *need },
		_ => Wait::Delay(DELAY * attempt),
	})
}

/// `fewer_threads` is the thread count of the next attempt when it is lower than this one's.
pub fn describe(wait: Wait, fewer_threads: Option<usize>) -> String {
	let waiting = match wait {
		Wait::Delay(delay) => format!("retrying in {} s", delay.as_secs()),
		Wait::Stable => "retrying once the file stops changing".to_owned(),
		Wait::Space { need } => format!("retrying once {} are free", DecimalBytes(need)),
	};
	match fewer_threads {
		Some(threads) => format!("{waiting} {}", threads_note(threads)),
		None => waiting,
	}
}

/// `wait_out` resolves to false when the wait ended without its condition being met, by Ctrl+C or by its time limit.
/// `on_retry` also gets the next attempt's thread count when it is lower.
pub async fn with_retries<R, W>(
	job: &mut Job,
	cancel: &CancellationToken,
	mut run: impl FnMut(Job) -> R,
	mut on_retry: impl FnMut(Wait, &JobError, Option<usize>),
	mut wait_out: impl FnMut(Wait) -> W,
) -> JobOutcome
where
	R: Future<Output = JobOutcome>,
	W: Future<Output = bool>,
{
	let mut attempt = 1;

	loop {
		let outcome = run(job.clone()).await;
		let JobOutcome::Failed(err) = &outcome else {
			return outcome;
		};
		let Some(wait) = next_wait(&err.kind, attempt) else {
			return outcome;
		};

		let threads = err.retry_threads(job.options.threads);
		on_retry(wait, err, err.fewer_threads(job.options.threads));
		if !wait_out(wait).await {
			if cancel.is_cancelled() {
				return JobOutcome::Failed(ErrorKind::Cancelled.into());
			}
			return outcome;
		}

		job.options.threads = threads;
		attempt += 1;
	}
}

pub async fn wait(wait: Wait, source: &Path, work_root: &Path, cancel: &CancellationToken) -> bool {
	match wait {
		Wait::Delay(delay) => sleep(delay, cancel).await,
		Wait::Stable => until_stable(source, STABLE_POLL, STABLE_FOR, STABLE_LIMIT, cancel).await,
		Wait::Space { need } => until_space(work_root, need, cancel).await,
	}
}

async fn sleep(duration: Duration, cancel: &CancellationToken) -> bool {
	tokio::select! {
		() = tokio::time::sleep(duration) => true,
		() = cancel.cancelled() => false,
	}
}

async fn until_stable(
	source: &Path,
	poll: Duration,
	stable_for: Duration,
	limit: Duration,
	cancel: &CancellationToken,
) -> bool {
	let mut last = signature(source);
	let mut still = Duration::ZERO;
	let mut waited = Duration::ZERO;

	while waited < limit {
		if !sleep(poll, cancel).await {
			return false;
		}
		waited += poll;

		let now = signature(source);
		if now.is_some() && now == last {
			still += poll;
			if still >= stable_for {
				return true;
			}
		} else {
			still = Duration::ZERO;
			last = now;
		}
	}
	false
}

async fn until_space(work_root: &Path, need: u64, cancel: &CancellationToken) -> bool {
	let folder = work_root.parent().unwrap_or(work_root);
	let mut waited = Duration::ZERO;

	loop {
		if free_space(folder).is_ok_and(|free| free >= need) {
			return true;
		}

		if waited >= SPACE_LIMIT || !sleep(SPACE_POLL, cancel).await {
			return false;
		}
		waited += SPACE_POLL;
	}
}

fn signature(path: &Path) -> Option<(u64, SystemTime)> {
	let meta = fs::metadata(path).ok()?;
	Some((meta.len(), meta.modified().ok()?))
}

#[cfg(test)]
mod tests {
	use std::time::Instant;

	use super::*;

	#[test]
	fn only_automatic_errors_are_retried_and_at_most_three_times() {
		assert_eq!(
			next_wait(&ErrorKind::Locked, 1),
			Some(Wait::Delay(Duration::from_secs(5)))
		);
		assert_eq!(
			next_wait(&ErrorKind::Locked, 2),
			Some(Wait::Delay(Duration::from_secs(10)))
		);
		assert_eq!(next_wait(&ErrorKind::Locked, 3), None);
		assert_eq!(next_wait(&ErrorKind::SourceChanged, 1), Some(Wait::Stable));
		assert_eq!(
			next_wait(&ErrorKind::InsufficientSpace { need: 10, free: 5 }, 1),
			Some(Wait::Space { need: 10 })
		);
		assert_eq!(next_wait(&ErrorKind::SourceDamaged("x".into()), 1), None);
		assert_eq!(next_wait(&ErrorKind::Cancelled, 1), None);
		assert_eq!(next_wait(&ErrorKind::EncodeFailed("quality".into()), 1), None);
	}

	#[tokio::test]
	async fn cancelling_during_a_retry_wait_stops_at_once() {
		let cancel = CancellationToken::new();
		let trigger = cancel.clone();
		tokio::spawn(async move {
			tokio::time::sleep(Duration::from_millis(100)).await;
			trigger.cancel();
		});
		let started = Instant::now();

		let waited = wait(
			Wait::Delay(Duration::from_secs(60)),
			Path::new("a"),
			Path::new("b"),
			&cancel,
		)
		.await;

		assert!(!waited);
		assert!(started.elapsed() < Duration::from_secs(2));
	}

	#[tokio::test]
	async fn a_file_that_holds_still_ends_the_stability_wait() {
		let dir = tempfile::tempdir().unwrap();
		let file = dir.path().join("a.flac");
		std::fs::write(&file, b"still").unwrap();

		let stable = until_stable(
			&file,
			Duration::from_millis(20),
			Duration::from_millis(100),
			Duration::from_secs(5),
			&CancellationToken::new(),
		)
		.await;

		assert!(stable);
	}

	fn job(threads: Option<usize>) -> Job {
		Job {
			source: "a.flac".into(),
			min_savings: 0.1,
			options: cryarchiver_engine::options::JobOptions {
				threads,
				..Default::default()
			},
			..Job::default()
		}
	}

	fn failed(kind: ErrorKind) -> JobOutcome {
		JobOutcome::Failed(kind.into())
	}

	fn done() -> JobOutcome {
		JobOutcome::Skipped { reason: "done".into() }
	}

	struct Script {
		threads: Vec<Option<usize>>,
		waits: Vec<Wait>,
		fewer: Vec<Option<usize>>,
	}

	async fn scripted(
		job: &mut Job,
		outcomes: Vec<JobOutcome>,
		waited: bool,
		cancel: &CancellationToken,
	) -> (JobOutcome, Script) {
		let mut outcomes = outcomes.into_iter();
		let mut threads = Vec::new();
		let mut waits = Vec::new();
		let mut fewer = Vec::new();

		let outcome = with_retries(
			job,
			cancel,
			|job: Job| {
				threads.push(job.options.threads);
				std::future::ready(outcomes.next().expect("no more attempts than scripted"))
			},
			|wait, _, next_threads| {
				waits.push(wait);
				fewer.push(next_threads);
			},
			|_| std::future::ready(waited),
		)
		.await;

		(outcome, Script { threads, waits, fewer })
	}

	#[tokio::test]
	async fn a_crash_halves_the_budget_the_engine_ran_the_job_on() {
		let mut job = job(None);
		let crash = |ran_on: usize| {
			JobOutcome::Failed(
				JobError::from(ErrorKind::ToolCrashed(
					"ffmpeg crashed: out of memory (exit code -12)".into(),
				))
				.ran_on(ran_on),
			)
		};

		let (outcome, script) = scripted(
			&mut job,
			vec![crash(4), crash(2), done()],
			true,
			&CancellationToken::new(),
		)
		.await;

		assert!(matches!(outcome, JobOutcome::Skipped { .. }), "{outcome:?}");
		assert_eq!(script.threads, [None, Some(2), Some(1)], "an image share of 4 threads");
		assert_eq!(script.fewer, [Some(2), Some(1)]);
	}

	#[test]
	fn a_retry_on_fewer_threads_says_so() {
		assert_eq!(
			describe(Wait::Delay(Duration::from_secs(5)), Some(2)),
			"retrying in 5 s with 2 threads"
		);
		assert_eq!(
			describe(Wait::Delay(Duration::from_secs(5)), Some(1)),
			"retrying in 5 s with 1 thread"
		);
		assert_eq!(describe(Wait::Stable, None), "retrying once the file stops changing");
	}

	#[tokio::test]
	async fn a_crash_halves_the_threads_and_the_third_failure_is_final() {
		let mut job = job(Some(8));
		let crash = || failed(ErrorKind::ToolCrashed("exception 0xC0000017 (out of memory)".into()));

		let (outcome, script) = scripted(
			&mut job,
			vec![crash(), crash(), crash()],
			true,
			&CancellationToken::new(),
		)
		.await;

		assert!(
			matches!(&outcome, JobOutcome::Failed(err) if matches!(err.kind, ErrorKind::ToolCrashed(_))),
			"{outcome:?}"
		);
		assert_eq!(script.threads, [Some(8), Some(4), Some(2)]);
		assert_eq!(
			script.waits,
			[
				Wait::Delay(Duration::from_secs(5)),
				Wait::Delay(Duration::from_secs(10))
			]
		);
	}

	#[tokio::test]
	async fn a_retry_that_succeeds_ends_the_attempts_and_other_errors_keep_the_threads() {
		let mut job = job(None);

		let (outcome, script) = scripted(
			&mut job,
			vec![failed(ErrorKind::Locked), done()],
			true,
			&CancellationToken::new(),
		)
		.await;
		let (unrecoverable, once) = scripted(
			&mut job,
			vec![failed(ErrorKind::SourceDamaged("x".into()))],
			true,
			&CancellationToken::new(),
		)
		.await;

		assert!(matches!(outcome, JobOutcome::Skipped { .. }), "{outcome:?}");
		assert_eq!(script.threads, [None, None]);
		assert!(matches!(unrecoverable, JobOutcome::Failed(_)), "{unrecoverable:?}");
		assert_eq!(once.threads.len(), 1);
		assert!(once.waits.is_empty());
	}

	#[tokio::test]
	async fn ctrl_c_during_a_wait_reports_cancelled_and_a_timeout_the_error() {
		let cancelled = CancellationToken::new();
		cancelled.cancel();

		let (stopped, _) = scripted(&mut job(None), vec![failed(ErrorKind::Locked)], false, &cancelled).await;
		let (timed_out, _) = scripted(
			&mut job(None),
			vec![failed(ErrorKind::SourceChanged)],
			false,
			&CancellationToken::new(),
		)
		.await;

		assert!(
			matches!(&stopped, JobOutcome::Failed(err) if err.kind == ErrorKind::Cancelled),
			"{stopped:?}"
		);
		assert!(
			matches!(&timed_out, JobOutcome::Failed(err) if err.kind == ErrorKind::SourceChanged),
			"{timed_out:?}"
		);
	}

	#[test]
	fn the_space_wait_names_a_readable_size() {
		assert_eq!(
			describe(Wait::Space { need: 4_300_000_000 }, None),
			"retrying once 4.30 GB are free"
		);
	}
}
