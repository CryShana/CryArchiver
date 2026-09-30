use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use cryarchiver_engine::commit::free_space;
use cryarchiver_engine::outcome::{ErrorClass, ErrorKind};
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

pub fn describe(wait: Wait) -> String {
	match wait {
		Wait::Delay(delay) => format!("retrying in {} s", delay.as_secs()),
		Wait::Stable => "retrying once the file stops changing".to_owned(),
		Wait::Space { need } => format!("retrying once {need} bytes are free"),
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
}
