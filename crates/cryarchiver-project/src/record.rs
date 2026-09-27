//! The values stored in the project file (project layer spec §3.3 and §5). They are `MessagePack` with named fields:
//! a newer version reads older records, missing fields take their defaults and unknown fields are ignored.

use std::fmt::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use cryarchiver_engine::commit::FileIdentity;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::keys::{BytesVisitor, RelPath};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(pub [u8; 32]);

impl Hash {
	pub fn to_hex(self) -> String {
		self.0.iter().fold(String::with_capacity(64), |mut hex, byte| {
			let _ = write!(hex, "{byte:02x}");
			hex
		})
	}
}

impl fmt::Debug for Hash {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.to_hex()[..16])
	}
}

impl Serialize for Hash {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		serializer.serialize_bytes(&self.0)
	}
}

impl<'de> Deserialize<'de> for Hash {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		let bytes = deserializer.deserialize_bytes(BytesVisitor)?;
		<[u8; 32]>::try_from(bytes.as_slice())
			.map(Self)
			.map_err(|_| serde::de::Error::invalid_length(bytes.len(), &"32 bytes"))
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
	pub volume: u64,
	pub index: u64,
}

impl From<FileIdentity> for Identity {
	fn from(identity: FileIdentity) -> Self {
		Self {
			volume: identity.volume,
			index: identity.index,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaClass {
	#[default]
	Unknown,
	Media,
	Other,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileRecord {
	pub size: u64,
	/// Nanoseconds since the Unix epoch.
	pub mtime_ns: i64,
	pub identity: Option<Identity>,
	pub class: MediaClass,
	pub window_hash: Option<Hash>,
	pub full_hash: Option<Hash>,
	/// The output of an earlier archive that a re-archive after a source change replaces.
	pub replaces: Option<RelPath>,
	pub state: FileState,
}

impl FileRecord {
	pub fn discovered(size: u64, mtime_ns: i64) -> Self {
		Self {
			size,
			mtime_ns,
			..Self::default()
		}
	}

	/// Stored hashes stay valid only while the size and modification time match (project layer spec §6.2).
	pub fn same_content_as(&self, size: u64, mtime_ns: i64) -> bool {
		self.size == size && self.mtime_ns == mtime_ns
	}
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum FileState {
	#[default]
	Discovered,
	Ready,
	Queued,
	InProgress {
		attempt: u32,
	},
	/// Written durably before the output's rename.
	Committing {
		attempt: u32,
		output: Output,
	},
	Done(Output),
	Issue {
		kind: IssueKind,
		attempt: u32,
		reason: String,
		/// The output a missing-or-changed issue is about.
		#[serde(default)]
		output: Option<Output>,
	},
	Excluded,
	/// The source was deleted by the user after its archive; the output stays.
	SourceGone(Output),
	/// Written durably before the original is deleted.
	DeletePending(Output),
	OriginalDeleted(Output),
}

impl FileState {
	pub fn output(&self) -> Option<&Output> {
		match self {
			Self::Committing { output, .. }
			| Self::Done(output)
			| Self::SourceGone(output)
			| Self::DeletePending(output)
			| Self::OriginalDeleted(output) => Some(output),
			Self::Discovered
			| Self::Ready
			| Self::Queued
			| Self::InProgress { .. }
			| Self::Issue { .. }
			| Self::Excluded => None,
		}
	}

	/// Done or resolved, as the progress rules count it (project layer spec §10.2).
	pub fn is_settled(&self) -> bool {
		matches!(
			self,
			Self::Done(_) | Self::SourceGone(_) | Self::DeletePending(_) | Self::OriginalDeleted(_) | Self::Excluded
		)
	}
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Output {
	pub how: How,
	pub path: RelPath,
	pub size: u64,
	/// `None` when the user accepted a changed output as it is: nothing can be verified against it.
	pub hash: Option<Hash>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum How {
	Encoded,
	Copied(CopyReason),
	Placeholder {
		canonical: RelPath,
		/// The canonical copy's archived content went missing before this placeholder could take it over. The content
		/// is lost, and the placeholder no longer depends on its canonical copy.
		#[serde(default, skip_serializing_if = "std::ops::Not::not")]
		lost: bool,
	},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyReason {
	NotMedia,
	KeptOriginal,
	/// The user chose "Copy original" for a file the engine couldn't archive.
	Chosen,
	/// A playlist, cue sheet or subtitle whose references were rewritten, so it differs from the source.
	Rewritten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueKind {
	Unrecoverable { damaged: bool },
	SourceUnreadable,
	OutputMissing,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderRecord {
	pub scanned: bool,
	pub timestamps_applied: bool,
	pub mtime_ns: Option<i64>,
	pub cycle: Option<CycleRecord>,
	/// The user acknowledged that the folder can't be read: it is left out until a listing of it succeeds.
	pub unreadable_acknowledged: bool,
}

impl FolderRecord {
	/// Nothing below it is followed: a link back into its own tree, or a folder left out as unreadable.
	pub fn left_out(&self) -> bool {
		self.cycle.is_some() || self.unreadable_acknowledged
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CycleRecord {
	pub acknowledged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FailureLog {
	pub error: String,
	pub log_tail: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UnknownRecord {
	pub acknowledged: bool,
}

pub fn mtime_ns(time: SystemTime) -> i64 {
	match time.duration_since(UNIX_EPOCH) {
		Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
		Err(before) => i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |nanos| -nanos),
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	fn done(path: &[u8]) -> FileRecord {
		FileRecord {
			state: FileState::Done(Output {
				how: How::Copied(CopyReason::NotMedia),
				path: RelPath::from_bytes(path.to_vec()),
				size: 3,
				hash: Some(Hash([7; 32])),
			}),
			..FileRecord::discovered(3, 1_000)
		}
	}

	#[test]
	fn records_round_trip_through_messagepack() {
		let record = FileRecord {
			identity: Some(Identity { volume: 1, index: 2 }),
			window_hash: Some(Hash([1; 32])),
			..done(b"a/b.txt")
		};

		let bytes = rmp_serde::to_vec_named(&record).unwrap();

		assert_eq!(rmp_serde::from_slice::<FileRecord>(&bytes).unwrap(), record);
	}

	#[test]
	fn unknown_fields_are_ignored_and_missing_ones_default() {
		#[derive(Serialize)]
		struct Newer {
			size: u64,
			mtime_ns: i64,
			colour: &'static str,
		}

		let bytes = rmp_serde::to_vec_named(&Newer {
			size: 5,
			mtime_ns: 9,
			colour: "red",
		})
		.unwrap();
		let record: FileRecord = rmp_serde::from_slice(&bytes).unwrap();

		assert_eq!(record, FileRecord::discovered(5, 9));
	}

	#[test]
	fn a_record_written_with_a_held_output_still_reads() {
		#[derive(Serialize)]
		struct Older {
			size: u64,
			mtime_ns: i64,
			state: FileState,
			held_output: Option<(RelPath, Output)>,
		}

		let held = done(b"a.txt").state.output().cloned();
		let bytes = rmp_serde::to_vec_named(&Older {
			size: 3,
			mtime_ns: 1_000,
			state: FileState::Queued,
			held_output: held.map(|output| (RelPath::from_bytes(b"a.txt".to_vec()), output)),
		})
		.unwrap();
		let record: FileRecord = rmp_serde::from_slice(&bytes).unwrap();

		assert_eq!(
			record,
			FileRecord {
				state: FileState::Queued,
				..FileRecord::discovered(3, 1_000)
			}
		);
	}

	#[test]
	fn a_placeholder_reads_whether_or_not_its_content_was_marked_lost() {
		#[derive(Serialize)]
		enum OlderHow {
			Placeholder { canonical: RelPath },
		}

		let canonical = RelPath::from_bytes(b"a.txt".to_vec());
		let older = rmp_serde::to_vec_named(&OlderHow::Placeholder {
			canonical: canonical.clone(),
		})
		.unwrap();
		let lost = How::Placeholder {
			canonical: canonical.clone(),
			lost: true,
		};

		assert_eq!(
			rmp_serde::from_slice::<How>(&older).unwrap(),
			How::Placeholder { canonical, lost: false }
		);
		assert_eq!(
			rmp_serde::from_slice::<How>(&rmp_serde::to_vec_named(&lost).unwrap()).unwrap(),
			lost
		);
	}

	#[test]
	fn settled_states_are_done_or_resolved() {
		assert!(done(b"a").state.is_settled());
		assert!(FileState::Excluded.is_settled());
		assert!(!FileState::Queued.is_settled());
		assert!(
			!FileState::Issue {
				kind: IssueKind::SourceUnreadable,
				attempt: 1,
				reason: String::new(),
				output: None,
			}
			.is_settled()
		);
	}

	#[test]
	fn hashes_are_reused_only_for_the_same_size_and_time() {
		let record = done(b"a");

		assert!(record.same_content_as(3, 1_000));
		assert!(!record.same_content_as(3, 1_001));
		assert!(!record.same_content_as(4, 1_000));
	}

	#[test]
	fn times_before_the_epoch_are_negative() {
		assert_eq!(mtime_ns(UNIX_EPOCH + Duration::from_secs(2)), 2_000_000_000);
		assert_eq!(mtime_ns(UNIX_EPOCH - Duration::from_secs(2)), -2_000_000_000);
	}
}
