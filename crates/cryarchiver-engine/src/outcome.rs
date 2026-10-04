use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::decision::Decision;
use crate::text::display_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
	AutoRetry,
	ManualRetry,
	Unrecoverable,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErrorKind {
	#[error("file is locked by another program")]
	Locked,
	#[error("source changed during the encode")]
	SourceChanged,
	#[error("not enough space: need {need} bytes, {free} free")]
	InsufficientSpace { need: u64, free: u64 },
	#[error("no progress for {0:?}")]
	Stalled(Duration),
	/// Names the tool: "ffprobe crashed: …".
	#[error("{0}")]
	ToolCrashed(String),
	#[error("i/o error at the destination: {0}")]
	DestinationIo(String),
	#[error("permission denied: {}", display_path(.0))]
	PermissionDenied(PathBuf),
	#[error("destination unavailable: {}", display_path(.0))]
	DestinationGone(PathBuf),
	/// A folder in the destination that leads elsewhere; nothing was written through it.
	#[error("not written: {0}")]
	UnsafeDestination(String),
	#[error("toolchain problem: {0}")]
	Toolchain(String),
	#[error("source unreadable: {0}")]
	SourceUnreadable(String),
	#[error("source damaged: {0}")]
	SourceDamaged(String),
	#[error("encode failed: {0}")]
	EncodeFailed(String),
	#[error("cancelled")]
	Cancelled,
}

impl ErrorKind {
	pub fn class(&self) -> ErrorClass {
		match self {
			Self::Locked
			| Self::SourceChanged
			| Self::InsufficientSpace { .. }
			| Self::Stalled(_)
			| Self::ToolCrashed(_)
			| Self::DestinationIo(_) => ErrorClass::AutoRetry,
			Self::PermissionDenied(_)
			| Self::DestinationGone(_)
			| Self::UnsafeDestination(_)
			| Self::Toolchain(_)
			| Self::Cancelled => ErrorClass::ManualRetry,
			Self::SourceUnreadable(_) | Self::SourceDamaged(_) | Self::EncodeFailed(_) => ErrorClass::Unrecoverable,
		}
	}

	/// The thread budget of the attempt after this error, from the budget of the one that failed: halved after a
	/// crash, which fewer threads may avoid, and otherwise kept. No budget counts as the whole machine.
	pub fn retry_threads(&self, threads: Option<usize>) -> Option<usize> {
		if !matches!(self, Self::ToolCrashed(_)) {
			return threads;
		}

		let current = threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(2, usize::from));
		Some((current / 2).max(1))
	}
}

#[derive(Debug, Clone, Copy)]
pub enum IoSide<'a> {
	Source(&'a Path),
	Destination(&'a Path),
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{kind}")]
pub struct JobError {
	pub kind: ErrorKind,
	pub log_tail: Vec<String>,
}

impl JobError {
	pub fn with_log(kind: ErrorKind, log_tail: Vec<String>) -> Self {
		Self { kind, log_tail }
	}

	pub fn is_disk_full(&self) -> bool {
		self.log_tail
			.iter()
			.any(|line| line.contains("No space left on device"))
	}

	pub fn io(side: IoSide<'_>, err: &io::Error) -> Self {
		if let Some(refused) = crate::workspace::UnsafeFolder::of(err) {
			return ErrorKind::UnsafeDestination(refused.to_string()).into();
		}

		let kind = match (side, err.kind()) {
			_ if is_sharing_violation(err) => ErrorKind::Locked,
			(IoSide::Source(path) | IoSide::Destination(path), io::ErrorKind::PermissionDenied) => {
				ErrorKind::PermissionDenied(path.to_path_buf())
			}
			(IoSide::Source(_), _) => ErrorKind::SourceUnreadable(err.to_string()),
			(IoSide::Destination(path), io::ErrorKind::NotFound) => ErrorKind::DestinationGone(path.to_path_buf()),
			(IoSide::Destination(_), _) => ErrorKind::DestinationIo(err.to_string()),
		};
		kind.into()
	}
}

impl From<ErrorKind> for JobError {
	fn from(kind: ErrorKind) -> Self {
		Self {
			kind,
			log_tail: Vec::new(),
		}
	}
}

pub fn is_sharing_violation(err: &io::Error) -> bool {
	const ERROR_SHARING_VIOLATION: i32 = 32;
	const ERROR_LOCK_VIOLATION: i32 = 33;

	cfg!(windows) && matches!(err.raw_os_error(), Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepReason {
	NotSmaller { src_bytes: u64, out_bytes: u64 },
	PredictedNotSmaller { src_bytes: u64, predicted_bytes: u64 },
	AlreadyEfficient(String),
	Unsupported(String),
	QualityUnreachable(String),
	InvalidIcc(String),
	DecodeBomb { megapixels: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
	Keep(KeepReason),
	Impossible(String),
}

impl From<KeepReason> for Refusal {
	fn from(reason: KeepReason) -> Self {
		Self::Keep(reason)
	}
}

#[derive(Debug, Clone)]
pub enum JobOutcome {
	Encoded {
		output: PathBuf,
		/// The source's size, or under a trim the window's share of it.
		src_bytes: u64,
		out_bytes: u64,
		warnings: Vec<String>,
		decision: Box<Decision>,
	},
	KeptOriginal {
		reason: KeepReason,
	},
	Skipped {
		reason: String,
	},
	Failed(JobError),
}

impl From<crate::process::RunError> for JobError {
	fn from(err: crate::process::RunError) -> Self {
		use crate::process::RunError;

		match err {
			RunError::Cancelled => ErrorKind::Cancelled.into(),
			RunError::Stalled(after) => ErrorKind::Stalled(after).into(),
			RunError::Spawn(err) => ErrorKind::Toolchain(err.to_string()).into(),
			err @ RunError::Io(_) => ErrorKind::ToolCrashed(err.to_string()).into(),
			RunError::CaptureLimit(limit) => ErrorKind::SourceDamaged(format!(
				"the decoded image is larger than its header declares ({limit} bytes)"
			))
			.into(),
			RunError::Crashed {
				tool,
				description,
				stderr_tail,
				..
			} => JobError::with_log(
				ErrorKind::ToolCrashed(format!("{tool} crashed: {description}")),
				stderr_tail,
			),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::io;
	use std::path::{Path, PathBuf};
	use std::time::Duration;

	use super::*;

	#[test]
	fn errors_are_classified() {
		assert_eq!(
			ErrorKind::InsufficientSpace { need: 10, free: 5 }.class(),
			ErrorClass::AutoRetry
		);
		assert_eq!(ErrorKind::SourceChanged.class(), ErrorClass::AutoRetry);
		assert_eq!(
			ErrorKind::Stalled(Duration::from_secs(600)).class(),
			ErrorClass::AutoRetry
		);
		assert_eq!(ErrorKind::DestinationIo("reset".into()).class(), ErrorClass::AutoRetry);
		assert_eq!(
			ErrorKind::PermissionDenied(PathBuf::from("x")).class(),
			ErrorClass::ManualRetry
		);
		assert_eq!(ErrorKind::Cancelled.class(), ErrorClass::ManualRetry);
		assert_eq!(
			ErrorKind::SourceDamaged("no EOI".into()).class(),
			ErrorClass::Unrecoverable
		);
	}

	#[test]
	fn a_crash_halves_the_threads_down_to_one_and_other_errors_keep_them() {
		let crash = ErrorKind::ToolCrashed("ffmpeg crashed: out of memory (exit code -12)".into());

		assert_eq!(crash.retry_threads(Some(8)), Some(4));
		assert_eq!(crash.retry_threads(Some(5)), Some(2));
		assert_eq!(crash.retry_threads(Some(1)), Some(1));
		assert!(crash.retry_threads(None).is_some_and(|threads| threads >= 1));
		assert_eq!(ErrorKind::Locked.retry_threads(Some(8)), Some(8));
		assert_eq!(ErrorKind::Locked.retry_threads(None), None);
	}

	#[test]
	fn insufficient_space_message_names_both_sizes() {
		let message = ErrorKind::InsufficientSpace {
			need: 4_300_000_000,
			free: 3_900_000_000,
		}
		.to_string();

		assert_eq!(message, "not enough space: need 4300000000 bytes, 3900000000 free");
	}

	#[test]
	fn io_errors_map_by_side_and_kind() {
		let denied = io::Error::from(io::ErrorKind::PermissionDenied);
		let missing = io::Error::from(io::ErrorKind::NotFound);
		let other = io::Error::other("network share went away");
		let source = IoSide::Source(Path::new("a.flac"));
		let destination = IoSide::Destination(Path::new("out"));

		assert_eq!(
			JobError::io(source, &denied).kind,
			ErrorKind::PermissionDenied(PathBuf::from("a.flac"))
		);
		assert!(matches!(
			JobError::io(source, &other).kind,
			ErrorKind::SourceUnreadable(_)
		));
		assert_eq!(
			JobError::io(destination, &missing).kind,
			ErrorKind::DestinationGone(PathBuf::from("out"))
		);
		assert_eq!(JobError::io(destination, &other).kind.class(), ErrorClass::AutoRetry);
	}

	#[test]
	fn a_folder_that_leads_elsewhere_is_named_and_never_retried_on_its_own() {
		let refused = crate::workspace::UnsafeFolder::error(Path::new("out"), crate::workspace::IS_A_LINK);

		let kind = JobError::io(IoSide::Destination(Path::new("out/a.avif")), &refused).kind;

		assert_eq!(
			kind,
			ErrorKind::UnsafeDestination(format!("out {}", crate::workspace::IS_A_LINK))
		);
		assert_eq!(kind.class(), ErrorClass::ManualRetry);
	}

	#[cfg(windows)]
	#[test]
	fn sharing_violations_are_locked() {
		let err = io::Error::from_raw_os_error(32);

		assert_eq!(
			JobError::io(IoSide::Source(Path::new("a.flac")), &err).kind,
			ErrorKind::Locked
		);
	}

	#[test]
	fn full_disks_are_recognised_from_the_log() {
		let full = JobError::with_log(
			ErrorKind::EncodeFailed("x".into()),
			vec!["[out#0/matroska @ 0000020f] Error writing trailer: No space left on device".into()],
		);

		assert!(full.is_disk_full());
		assert!(!JobError::from(ErrorKind::EncodeFailed("x".into())).is_disk_full());
	}
}
