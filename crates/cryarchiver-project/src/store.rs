//! The project file: one redb database (project layer spec §3). Write-ahead intents are durable before their action
//! runs; every other change waits in a batch that commits every 1000 changes or 250 ms, whichever comes first.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cryarchiver_engine::commit::{FileIdentity, file_id, retry_transient};
use cryarchiver_engine::quality::memory::GroupStats;
use redb::{
	Database, ReadOnlyDatabase, ReadOnlyTable, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::keys::RelPath;
use crate::record::{
	CopyReason, FailureLog, FileRecord, FileState, FolderRecord, Hash, How, Identity, IssueKind, MediaClass, Output,
	UnknownRecord,
};
use crate::settings::Settings;

pub const FORMAT_VERSION: u32 = 2;
pub const BATCH_CHANGES: usize = 1000;
pub const BATCH_INTERVAL: Duration = Duration::from_millis(250);
pub const LOG_TAIL_LINES: usize = 100;
pub const LOG_LINE_CHARS: usize = 400;
pub const ATTEMPT_LOGS_KEPT: u32 = 5;
pub const LOG_BUDGET_BYTES: u64 = 32 << 20;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const FILES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("files");
const FOLDERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("folders");
const LOGS: TableDefinition<(&[u8], u32), &[u8]> = TableDefinition::new("logs");
const UNKNOWN: TableDefinition<&[u8], &[u8]> = TableDefinition::new("unknown");
const FOLDER_IDS: TableDefinition<u32, &[u8]> = TableDefinition::new("folder_ids");
const ROOT_FOLDER: u32 = 0;

const FORMAT_KEY: &str = "format";
const APP_VERSION_KEY: &str = "app_version";
const CREATED_KEY: &str = "created";
const SETTINGS_KEY: &str = "settings";
const QUALITY_PREFIX: &str = "quality/";
const DICTIONARY_KEY: &str = "dictionary";

const RAW: u8 = 0;
const LZ4: u8 = 1;
const MAX_RECORD_BYTES: usize = 16 << 20;
const DICTIONARY_LOG_LINES: &[&str] = &[
	"Error while decoding stream #0:0: Invalid data found when processing input",
	"[h264 @ 0x0] error while decoding MB 0 0, bytestream -1",
	"[matroska,webm @ 0x0] ",
	"Svt[info]: ",
	"frame=    0 fps=0.0 q=0.0 size=       0KiB time=00:00:00.00 bitrate=N/A speed=   0x",
	"Stream mapping:",
	"Input #0, ",
	"Output #0, matroska, to ",
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
	#[error("the project is already open in another window or run")]
	AlreadyOpen,
	#[error("{0} already exists")]
	Exists(String),
	#[error("the project was made by a newer CryArchiver (format {0}); update CryArchiver to open it")]
	TooNew(u32),
	#[error("the project file needs an upgrade this version can't do (redb format {0})")]
	UpgradeRequired(u8),
	#[error("the project file is damaged: {0}")]
	Damaged(String),
	#[error("the project file couldn't be upgraded to this version's format and was left as it was: {0}")]
	UpgradeFailed(String),
	#[error("the project wasn't closed cleanly; opening it recovers it")]
	NeedsRecovery,
	#[error("the project file is missing")]
	Missing,
	#[error("unavailable (drive or share offline)")]
	Unavailable,
	#[error("not a project file")]
	NotAProjectFile,
	#[error(
		"the project file has {0} names (hard links), and writing it would change it under every other name too; it is neither opened nor made while it has more than one"
	)]
	HardLinked(u64),
	#[error(transparent)]
	Io(#[from] io::Error),
	#[error("project file error: {0}")]
	Database(redb::Error),
}

impl From<redb::Error> for StoreError {
	fn from(err: redb::Error) -> Self {
		match err {
			redb::Error::DatabaseAlreadyOpen => Self::AlreadyOpen,
			redb::Error::UpgradeRequired(version) => Self::UpgradeRequired(version),
			other => Self::Database(other),
		}
	}
}

macro_rules! from_redb {
	($($error:ty),+) => {
		$(impl From<$error> for StoreError {
			fn from(err: $error) -> Self {
				redb::Error::from(err).into()
			}
		})+
	};
}

from_redb!(
	redb::DatabaseError,
	redb::TransactionError,
	redb::TableError,
	redb::StorageError,
	redb::CommitError,
	redb::CompactionError
);

impl From<rmp_serde::encode::Error> for StoreError {
	fn from(err: rmp_serde::encode::Error) -> Self {
		Self::Damaged(err.to_string())
	}
}

impl From<rmp_serde::decode::Error> for StoreError {
	fn from(err: rmp_serde::decode::Error) -> Self {
		Self::Damaged(err.to_string())
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
	pub format: u32,
	pub app_version: String,
	/// Seconds since the Unix epoch.
	pub created: u64,
	pub settings: Settings,
}

/// Everything the project task rebuilds its in-memory state from.
#[derive(Debug, Default)]
pub struct Loaded {
	pub files: Vec<(RelPath, FileRecord)>,
	pub folders: Vec<(RelPath, FolderRecord)>,
	pub unknown: Vec<(RelPath, UnknownRecord)>,
	/// The highest attempt with a failure log, per file.
	pub last_attempts: HashMap<RelPath, u32>,
	pub quality: Vec<(String, GroupStats)>,
	/// One line for each learned-quality entry that was left out.
	pub ignored_quality: Vec<String>,
	/// The file wasn't closed cleanly, and opening it repaired it.
	pub recovered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSize {
	pub entries: u64,
	pub stored_bytes: u64,
	pub metadata_bytes: u64,
	pub fragmented_bytes: u64,
}

fn size_of(table: &impl ReadableTableMetadata) -> Result<TableSize, StoreError> {
	let stats = table.stats()?;
	Ok(TableSize {
		entries: table.len()?,
		stored_bytes: stats.stored_bytes(),
		metadata_bytes: stats.metadata_bytes(),
		fragmented_bytes: stats.fragmented_bytes(),
	})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
	Meta,
	Files,
	Folders,
	Logs(u32),
	Unknown,
	FolderIds,
}

#[derive(Debug)]
struct Change {
	table: Table,
	key: Vec<u8>,
	value: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct Codec {
	dictionary: Vec<u8>,
}

impl Codec {
	fn built_in() -> Self {
		Self {
			dictionary: built_in_dictionary(),
		}
	}

	fn pack(&self, bytes: &[u8]) -> Vec<u8> {
		let packed = (bytes.len() <= MAX_RECORD_BYTES)
			.then(|| lz4_flex::block::compress_prepend_size_with_dict(bytes, &self.dictionary));
		let (tag, body) = match &packed {
			Some(packed) if packed.len() < bytes.len() => (LZ4, packed.as_slice()),
			_ => (RAW, bytes),
		};

		let mut stored = Vec::with_capacity(body.len() + 1);
		stored.push(tag);
		stored.extend_from_slice(body);
		stored
	}

	fn unpack(&self, stored: &[u8]) -> Result<Vec<u8>, StoreError> {
		match stored.split_first() {
			Some((&RAW, bytes)) => Ok(bytes.to_vec()),
			Some((&LZ4, packed)) => {
				let (size, body) = packed
					.split_first_chunk::<4>()
					.ok_or_else(|| damaged("a compressed record is cut short"))?;
				let size = usize::try_from(u32::from_le_bytes(*size)).unwrap_or(usize::MAX);
				if size > MAX_RECORD_BYTES {
					return Err(damaged("a compressed record claims more than 16 MiB"));
				}

				let bytes = lz4_flex::block::decompress_with_dict(body, size, &self.dictionary)
					.map_err(|err| StoreError::Damaged(format!("a compressed record can't be read: {err}")))?;
				if bytes.len() != size {
					return Err(damaged("a compressed record is shorter than it claims"));
				}
				Ok(bytes)
			}
			_ => Err(damaged("a record has an unknown encoding")),
		}
	}
}

fn damaged(reason: &str) -> StoreError {
	StoreError::Damaged(reason.to_owned())
}

fn in_table(table: &'static str) -> impl FnOnce(StoreError) -> StoreError {
	move |err| match err {
		StoreError::Damaged(reason) => StoreError::Damaged(format!("{table}: {reason}")),
		other => other,
	}
}

fn in_entry<'a>(table: &'static str, path: &'a RelPath) -> impl FnOnce(StoreError) -> StoreError + 'a {
	move |err| match err {
		StoreError::Damaged(reason) => StoreError::Damaged(format!("{table} entry {path:?}: {reason}")),
		other => other,
	}
}

/// Samples of every record kind in their serialized form: field names and common values compress to back-references.
/// Each project file keeps its own copy in `meta`, so a later build can change these samples freely.
fn built_in_dictionary() -> Vec<u8> {
	let hash = Some(Hash([0; 32]));
	let output = Output {
		how: How::Encoded,
		path: RelPath::from_bytes(b"Photos/2020/IMG_0001.cry.avif".to_vec()),
		size: 1_000_000,
		hash,
	};
	let files = [
		FileRecord {
			identity: Some(Identity { volume: 1, index: 1 }),
			class: MediaClass::Media,
			window_hash: hash,
			full_hash: hash,
			state: FileState::Done(output.clone()),
			..FileRecord::discovered(1_000_000, 1_600_000_000_000_000_000)
		},
		FileRecord {
			state: FileState::Committing {
				attempt: 1,
				output: output.clone(),
			},
			..FileRecord::default()
		},
		FileRecord {
			state: FileState::Issue {
				kind: IssueKind::Unrecoverable { damaged: true },
				attempt: 1,
				reason: "source damaged: ".into(),
				output: None,
			},
			..FileRecord::default()
		},
		FileRecord {
			state: FileState::OriginalDeleted(Output {
				how: How::Copied(CopyReason::KeptOriginal),
				..output
			}),
			..FileRecord::default()
		},
	];
	let folder = FolderRecord {
		scanned: true,
		timestamps_applied: true,
		mtime_ns: Some(0),
		..FolderRecord::default()
	};
	let log = FailureLog {
		error: "encode failed: ".into(),
		log_tail: DICTIONARY_LOG_LINES.iter().map(|line| (*line).to_owned()).collect(),
	};

	let mut dictionary = Vec::new();
	for file in &files {
		dictionary.extend(rmp_serde::to_vec_named(file).unwrap_or_default());
	}
	dictionary.extend(rmp_serde::to_vec_named(&folder).unwrap_or_default());
	dictionary.extend(rmp_serde::to_vec_named(&log).unwrap_or_default());
	dictionary
}

#[derive(Debug, Default)]
struct FolderIds {
	ids: HashMap<RelPath, u32>,
	paths: HashMap<u32, RelPath>,
	next: u32,
}

impl FolderIds {
	fn with_root() -> Self {
		let mut folders = Self::default();
		folders.insert(ROOT_FOLDER, RelPath::root());
		folders
	}

	fn insert(&mut self, id: u32, path: RelPath) {
		self.next = self.next.max(id.saturating_add(1));
		self.ids.insert(path.clone(), id);
		self.paths.insert(id, path);
	}

	fn get(&self, folder: &RelPath) -> Option<u32> {
		self.ids.get(folder).copied()
	}

	/// The folder's id, and whether it was new.
	fn assign(&mut self, folder: &RelPath) -> Result<(u32, bool), StoreError> {
		if let Some(id) = self.get(folder) {
			return Ok((id, false));
		}

		let id = self.next;
		if self.paths.contains_key(&id) {
			return Err(io::Error::other("the project has more folders than it can number").into());
		}

		self.insert(id, folder.clone());
		Ok((id, true))
	}

	fn key_for(&mut self, path: &RelPath) -> Result<Vec<u8>, StoreError> {
		let (id, _) = self.assign(&path.parent().unwrap_or_default())?;
		Ok(file_key(id, path.name_bytes()))
	}

	fn path_of(&self, key: &[u8]) -> Result<RelPath, StoreError> {
		let (id, name) = key
			.split_first_chunk::<4>()
			.ok_or_else(|| damaged("a file key is cut short"))?;
		let folder = self
			.paths
			.get(&u32::from_be_bytes(*id))
			.ok_or_else(|| damaged("a file names a folder the project doesn't know"))?;
		Ok(folder.join_bytes(name))
	}
}

fn file_key(folder: u32, name: &[u8]) -> Vec<u8> {
	let mut key = folder.to_be_bytes().to_vec();
	key.extend_from_slice(name);
	key
}

fn folder_id(key: &[u8]) -> Result<u32, StoreError> {
	<[u8; 4]>::try_from(key)
		.map(u32::from_be_bytes)
		.map_err(|_| damaged("a folder id is malformed"))
}

fn read_layout(db: &impl ReadableDatabase) -> Result<(Codec, FolderIds), StoreError> {
	let txn = db.begin_read()?;
	let dictionary = txn
		.open_table(META)?
		.get(DICTIONARY_KEY)?
		.map(|value| value.value().to_vec())
		.ok_or_else(|| damaged("the dictionary entry is missing"))?;

	let mut folders = FolderIds::default();
	for entry in txn.open_table(FOLDER_IDS)?.iter()? {
		let (id, path) = entry?;
		folders.insert(id.value(), RelPath::from_bytes(path.value().to_vec()));
	}
	Ok((Codec { dictionary }, folders))
}

pub struct Store {
	db: Database,
	pending: Vec<Change>,
	oldest_pending: Option<Instant>,
	codec: Codec,
	folders: FolderIds,
}

impl Store {
	/// Refuses anything already at `path`, a dangling link too, and creates the file itself, never a link's target.
	pub fn create(path: &Path, settings: &Settings) -> Result<Self, StoreError> {
		let exists = || StoreError::Exists(cryarchiver_engine::text::display_path(path));
		if fs::symlink_metadata(path).is_ok() {
			return Err(exists());
		}

		let file = match create_new_file(path) {
			Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Err(exists()),
			result => result?,
		};
		refuse_other_names(&file)?;
		let db = Database::builder().create_file(file)?;
		let codec = Codec::built_in();
		write_new(&db, settings, &codec)?;

		Ok(Self {
			db,
			pending: Vec::new(),
			oldest_pending: None,
			codec,
			folders: FolderIds::with_root(),
		})
	}

	/// An older format is upgraded first, into a copy that replaces the file only once it is complete, synced and
	/// read back; until then the file stays as it was.
	pub fn open(path: &Path) -> Result<(Self, Meta, Loaded), StoreError> {
		require_a_file(path)?;
		refuse_other_names(&open_in_place(path)?)?;

		let (db, recovered) = open_database(path)?;
		let format = read_meta(&db)?.format;
		if format > FORMAT_VERSION {
			return Err(StoreError::TooNew(format));
		}

		let db = if format < FORMAT_VERSION {
			let real = real_file(path).map_err(upgrade_failed)?;
			remove_stale_upgrade(&real).map_err(upgrade_failed)?;
			drop(db);

			migrate(&real).map_err(upgrade_failed)?;
			open_database(path)?.0
		} else {
			db
		};

		let (codec, folders) = read_layout(&db)?;
		let meta = read_meta(&db)?;
		let store = Self {
			db,
			pending: Vec::new(),
			oldest_pending: None,
			codec,
			folders,
		};
		let mut loaded = store.load()?;
		loaded.recovered = recovered;
		Ok((store, meta, loaded))
	}

	fn load(&self) -> Result<Loaded, StoreError> {
		let txn = self.db.begin_read()?;
		let mut loaded = Loaded::default();

		for entry in txn.open_table(FILES)?.iter()? {
			let (key, value) = entry?;
			let path = self.folders.path_of(key.value()).map_err(in_table("files"))?;
			let record = self.decode_value(value.value()).map_err(in_entry("files", &path))?;
			loaded.files.push((path, record));
		}

		for entry in txn.open_table(FOLDERS)?.iter()? {
			let (key, value) = entry?;
			let path = RelPath::from_bytes(key.value().to_vec());
			let record = self.decode_value(value.value()).map_err(in_entry("folders", &path))?;
			loaded.folders.push((path, record));
		}

		for entry in txn.open_table(UNKNOWN)?.iter()? {
			let (key, value) = entry?;
			let path = self.folders.path_of(key.value()).map_err(in_table("unknown"))?;
			let record = self.decode_value(value.value()).map_err(in_entry("unknown", &path))?;
			loaded.unknown.push((path, record));
		}

		for entry in txn.open_table(LOGS)?.iter()? {
			let (key, _) = entry?;
			let (path, attempt) = key.value();
			let path = self.folders.path_of(path).map_err(in_table("logs"))?;
			let last = loaded.last_attempts.entry(path).or_default();
			*last = (*last).max(attempt);
		}

		// Learned quality is only a cache: an entry this build can't read or use, damaged, hand-edited or from a later
		// build, is set aside on its own rather than keeping the project from opening.
		for entry in txn.open_table(META)?.iter()? {
			let (key, value) = entry?;
			let Some(quality_key) = key.value().strip_prefix(QUALITY_PREFIX) else {
				continue;
			};

			let problem = match decode::<GroupStats>(value.value()) {
				Ok(stats) => match stats.problem(quality_key) {
					None => {
						loaded.quality.push((quality_key.to_owned(), stats));
						continue;
					}
					Some(problem) => problem,
				},
				Err(err) => err.to_string(),
			};
			loaded
				.ignored_quality
				.push(format!("the learned quality for {quality_key} was ignored: {problem}"));
		}
		Ok(loaded)
	}

	pub fn set_file(&mut self, path: &RelPath, record: &FileRecord) -> Result<(), StoreError> {
		let key = self.file_key(path)?;
		self.put(Table::Files, key, record)
	}

	pub fn remove_file(&mut self, path: &RelPath) {
		if let Some(key) = self.known_file_key(path) {
			self.remove(Table::Files, key);
		}
	}

	pub fn set_folder(&mut self, path: &RelPath, record: &FolderRecord) -> Result<(), StoreError> {
		self.put(Table::Folders, path.as_bytes().to_vec(), record)
	}

	pub fn set_log(&mut self, path: &RelPath, attempt: u32, log: &FailureLog) -> Result<(), StoreError> {
		let key = self.file_key(path)?;
		if let Some(dropped) = attempt.checked_sub(ATTEMPT_LOGS_KEPT).filter(|dropped| *dropped > 0) {
			self.remove(Table::Logs(dropped), key.clone());
		}
		self.put(Table::Logs(attempt), key, &trimmed(log))
	}

	pub fn set_unknown(&mut self, path: &RelPath, record: &UnknownRecord) -> Result<(), StoreError> {
		let key = self.file_key(path)?;
		self.put(Table::Unknown, key, record)
	}

	pub fn remove_unknown(&mut self, path: &RelPath) {
		if let Some(key) = self.known_file_key(path) {
			self.remove(Table::Unknown, key);
		}
	}

	pub fn compact(&mut self) -> Result<bool, StoreError> {
		self.flush()?;
		Ok(self.db.compact()?)
	}

	/// Removes failure logs until the log table fits `budget`: first the logs of files that aren't failing any more,
	/// then older attempts of failing files. The newest log of a failing file always stays.
	pub fn prune_logs(&mut self, failing: &HashSet<RelPath>, budget: u64) -> Result<usize, StoreError> {
		self.flush()?;

		let mut logs: Vec<(Vec<u8>, u32, u64)> = Vec::new();
		{
			let txn = self.db.begin_read()?;
			for entry in txn.open_table(LOGS)?.iter()? {
				let (key, value) = entry?;
				let (path, attempt) = key.value();
				logs.push((
					path.to_vec(),
					attempt,
					u64::try_from(value.value().len()).unwrap_or(u64::MAX),
				));
			}
		}
		let mut total: u64 = logs.iter().map(|(_, _, bytes)| bytes).sum();
		if total <= budget {
			return Ok(0);
		}

		let kept: HashSet<Vec<u8>> = failing.iter().filter_map(|path| self.known_file_key(path)).collect();
		let mut newest: HashMap<&[u8], u32> = HashMap::new();
		for (key, attempt, _) in &logs {
			let last = newest.entry(key.as_slice()).or_default();
			*last = (*last).max(*attempt);
		}
		let mut removable: Vec<&(Vec<u8>, u32, u64)> = logs
			.iter()
			.filter(|(key, attempt, _)| !kept.contains(key) || newest.get(key.as_slice()) != Some(attempt))
			.collect();
		removable.sort_by_key(|(key, attempt, _)| (kept.contains(key), *attempt));

		let mut removed = 0;
		for (key, attempt, bytes) in removable {
			if total <= budget {
				break;
			}

			self.remove(Table::Logs(*attempt), key.clone());
			total = total.saturating_sub(*bytes);
			removed += 1;
		}
		self.flush()?;
		Ok(removed)
	}

	pub fn set_settings(&mut self, settings: &Settings) -> Result<(), StoreError> {
		self.put(Table::Meta, SETTINGS_KEY.as_bytes().to_vec(), settings)
	}

	pub fn set_quality(&mut self, key: &str, stats: &GroupStats) -> Result<(), StoreError> {
		self.put(Table::Meta, format!("{QUALITY_PREFIX}{key}").into_bytes(), stats)
	}

	pub fn remove_quality(&mut self, key: &str) {
		self.remove(Table::Meta, format!("{QUALITY_PREFIX}{key}").into_bytes());
	}

	/// A write-ahead intent: commits it together with everything batched so far, durably, before returning.
	pub fn write_intent(&mut self, path: &RelPath, record: &FileRecord) -> Result<(), StoreError> {
		self.set_file(path, record)?;
		self.flush()
	}

	pub fn pending_changes(&self) -> usize {
		self.pending.len()
	}

	pub fn table_sizes(&self) -> Result<Vec<(&'static str, TableSize)>, StoreError> {
		let txn = self.db.begin_read()?;
		Ok(vec![
			("meta", size_of(&txn.open_table(META)?)?),
			("files", size_of(&txn.open_table(FILES)?)?),
			("folders", size_of(&txn.open_table(FOLDERS)?)?),
			("logs", size_of(&txn.open_table(LOGS)?)?),
			("unknown", size_of(&txn.open_table(UNKNOWN)?)?),
			("folder_ids", size_of(&txn.open_table(FOLDER_IDS)?)?),
		])
	}

	pub fn flush_if_due(&mut self, now: Instant) -> Result<bool, StoreError> {
		let due = self.pending.len() >= BATCH_CHANGES
			|| self
				.oldest_pending
				.is_some_and(|oldest| now.duration_since(oldest) >= BATCH_INTERVAL);
		if due {
			self.flush()?;
		}
		Ok(due)
	}

	pub fn flush(&mut self) -> Result<(), StoreError> {
		if self.pending.is_empty() {
			return Ok(());
		}

		let txn = self.db.begin_write()?;
		{
			let mut meta = txn.open_table(META)?;
			let mut files = txn.open_table(FILES)?;
			let mut folders = txn.open_table(FOLDERS)?;
			let mut logs = txn.open_table(LOGS)?;
			let mut unknown = txn.open_table(UNKNOWN)?;
			let mut folder_ids = txn.open_table(FOLDER_IDS)?;

			for change in &self.pending {
				let key = change.key.as_slice();
				match (change.table, &change.value) {
					(Table::Meta, Some(value)) => {
						meta.insert(std::str::from_utf8(key).unwrap_or_default(), value.as_slice())?;
					}
					(Table::Meta, None) => {
						meta.remove(std::str::from_utf8(key).unwrap_or_default())?;
					}
					(Table::Files, Some(value)) => {
						files.insert(key, value.as_slice())?;
					}
					(Table::Files, None) => {
						files.remove(key)?;
					}
					(Table::Folders, Some(value)) => {
						folders.insert(key, value.as_slice())?;
					}
					(Table::Folders, None) => {
						folders.remove(key)?;
					}
					(Table::Logs(attempt), Some(value)) => {
						logs.insert((key, attempt), value.as_slice())?;
					}
					(Table::Logs(attempt), None) => {
						logs.remove((key, attempt))?;
					}
					(Table::Unknown, Some(value)) => {
						unknown.insert(key, value.as_slice())?;
					}
					(Table::Unknown, None) => {
						unknown.remove(key)?;
					}
					(Table::FolderIds, Some(value)) => {
						folder_ids.insert(folder_id(key)?, value.as_slice())?;
					}
					(Table::FolderIds, None) => {
						folder_ids.remove(folder_id(key)?)?;
					}
				}
			}
		}
		txn.commit()?;

		self.pending.clear();
		self.oldest_pending = None;
		Ok(())
	}

	pub fn read_log(&self, path: &RelPath, attempt: u32) -> Result<Option<FailureLog>, StoreError> {
		let Some(key) = self.known_file_key(path) else {
			return Ok(None);
		};

		let batched = self
			.pending
			.iter()
			.rev()
			.find(|change| change.table == Table::Logs(attempt) && change.key == key);
		if let Some(change) = batched {
			return change
				.value
				.as_deref()
				.map(|value| self.decode_value(value))
				.transpose();
		}

		let txn = self.db.begin_read()?;
		let logs = txn.open_table(LOGS)?;
		logs.get((key.as_slice(), attempt))?
			.map(|value| self.decode_value(value.value()))
			.transpose()
	}

	fn file_key(&mut self, path: &RelPath) -> Result<Vec<u8>, StoreError> {
		let folder = path.parent().unwrap_or_default();
		let (id, new) = self.folders.assign(&folder)?;
		if new {
			self.push(Change {
				table: Table::FolderIds,
				key: id.to_be_bytes().to_vec(),
				value: Some(folder.as_bytes().to_vec()),
			});
		}
		Ok(file_key(id, path.name_bytes()))
	}

	fn known_file_key(&self, path: &RelPath) -> Option<Vec<u8>> {
		let id = self.folders.get(&path.parent().unwrap_or_default())?;
		Some(file_key(id, path.name_bytes()))
	}

	fn put<T: Serialize + ?Sized>(&mut self, table: Table, key: Vec<u8>, value: &T) -> Result<(), StoreError> {
		let bytes = rmp_serde::to_vec_named(value)?;
		let value = if table == Table::Meta {
			bytes
		} else {
			self.codec.pack(&bytes)
		};
		self.push(Change {
			table,
			key,
			value: Some(value),
		});
		Ok(())
	}

	fn decode_value<T: DeserializeOwned>(&self, stored: &[u8]) -> Result<T, StoreError> {
		decode(&self.codec.unpack(stored)?)
	}

	fn remove(&mut self, table: Table, key: Vec<u8>) {
		self.push(Change {
			table,
			key,
			value: None,
		});
	}

	fn push(&mut self, change: Change) {
		self.oldest_pending.get_or_insert_with(Instant::now);
		self.pending.push(change);
	}
}

fn trimmed(log: &FailureLog) -> FailureLog {
	let skip = log.log_tail.len().saturating_sub(LOG_TAIL_LINES);
	FailureLog {
		error: log.error.chars().take(LOG_LINE_CHARS).collect(),
		log_tail: log.log_tail[skip..]
			.iter()
			.map(|line| line.chars().take(LOG_LINE_CHARS).collect())
			.collect(),
	}
}

/// Creating through a dangling link would create its target, and on Windows `create_new` alone still does: the link is
/// opened in its own place, and the handle must show a plain file.
#[cfg(windows)]
fn create_new_file(path: &Path) -> io::Result<fs::File> {
	use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

	use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT};

	let file = fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create_new(true)
		.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
		.open(path)?;
	if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
		return Err(io::ErrorKind::AlreadyExists.into());
	}
	Ok(file)
}

#[cfg(unix)]
fn create_new_file(path: &Path) -> io::Result<fs::File> {
	use std::os::unix::fs::OpenOptionsExt;

	fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create_new(true)
		.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
		.open(path)
}

fn require_a_file(path: &Path) -> Result<(), StoreError> {
	if path.is_file() {
		return Ok(());
	}
	Err(io::Error::new(io::ErrorKind::NotFound, "no project file at this path").into())
}

/// Another name for the project file, inside a source say, would have that file written too.
fn refuse_other_names(file: &fs::File) -> Result<(), StoreError> {
	match link_count(file)? {
		0 | 1 => Ok(()),
		names => Err(StoreError::HardLinked(names)),
	}
}

/// Opens the entry itself, never a link's target, for reading its information only.
#[cfg(windows)]
fn open_in_place(path: &Path) -> io::Result<fs::File> {
	use std::os::windows::fs::OpenOptionsExt;

	use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

	fs::OpenOptions::new()
		.access_mode(0)
		.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
		.open(path)
}

#[cfg(unix)]
fn open_in_place(path: &Path) -> io::Result<fs::File> {
	use std::os::unix::fs::OpenOptionsExt;

	fs::OpenOptions::new()
		.read(true)
		.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
		.open(path)
}

#[cfg(windows)]
fn link_count(file: &fs::File) -> io::Result<u64> {
	use std::os::windows::io::AsRawHandle;

	use windows::Win32::Foundation::HANDLE;
	use windows::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};

	let mut info = BY_HANDLE_FILE_INFORMATION::default();
	// SAFETY: the handle is owned by `file` and valid for the call.
	unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }
		.map_err(|err| io::Error::from_raw_os_error(err.code().0 & 0xFFFF))?;
	Ok(u64::from(info.nNumberOfLinks))
}

#[cfg(unix)]
fn link_count(file: &fs::File) -> io::Result<u64> {
	use std::os::unix::fs::MetadataExt;

	Ok(file.metadata()?.nlink())
}

fn write_new(db: &Database, settings: &Settings, codec: &Codec) -> Result<(), StoreError> {
	let created = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |since| since.as_secs());
	let txn = db.begin_write()?;
	{
		let mut meta = txn.open_table(META)?;
		meta.insert(FORMAT_KEY, rmp_serde::to_vec_named(&FORMAT_VERSION)?.as_slice())?;
		meta.insert(
			APP_VERSION_KEY,
			rmp_serde::to_vec_named(cryarchiver_engine::VERSION)?.as_slice(),
		)?;
		meta.insert(CREATED_KEY, rmp_serde::to_vec_named(&created)?.as_slice())?;
		meta.insert(SETTINGS_KEY, rmp_serde::to_vec_named(settings)?.as_slice())?;
		meta.insert(DICTIONARY_KEY, codec.dictionary.as_slice())?;
		txn.open_table(FILES)?;
		txn.open_table(FOLDERS)?;
		txn.open_table(LOGS)?;
		txn.open_table(UNKNOWN)?;
		txn.open_table(FOLDER_IDS)?.insert(ROOT_FOLDER, b"".as_slice())?;
	}
	txn.commit()?;
	Ok(())
}

fn open_database(path: &Path) -> Result<(Database, bool), StoreError> {
	let repaired = Arc::new(AtomicBool::new(false));
	let flag = Arc::clone(&repaired);
	let db = Database::builder()
		.set_repair_callback(move |_| flag.store(true, Ordering::SeqCst))
		.open(path)?;
	Ok((db, repaired.load(Ordering::SeqCst)))
}

fn upgrade_path(path: &Path) -> PathBuf {
	let mut name = path.file_name().unwrap_or_default().to_os_string();
	name.push(".upgrade");
	path.with_file_name(name)
}

/// The file a linked project file leads to: the upgrade writes its copy beside that file and renames it over that
/// file, never over the link.
fn real_file(path: &Path) -> Result<PathBuf, StoreError> {
	if fs::symlink_metadata(path)?.file_type().is_symlink() {
		return Ok(fs::canonicalize(path)?);
	}
	Ok(path.to_path_buf())
}

/// Every error of an upgrade says that the project file was left as it was, except a refusal the caller acts on.
fn upgrade_failed(err: StoreError) -> StoreError {
	let full = match &err {
		StoreError::Io(io) | StoreError::Database(redb::Error::Io(io)) => io.kind() == io::ErrorKind::StorageFull,
		_ => false,
	};

	match err {
		StoreError::UpgradeFailed(_) | StoreError::AlreadyOpen | StoreError::TooNew(_) => err,
		_ if full => StoreError::UpgradeFailed(
			"there isn't enough free space for the upgraded copy, which takes about as much as the project file".into(),
		),
		other => StoreError::UpgradeFailed(other.to_string()),
	}
}

/// An upgrade holds this lock on its copy from the moment the copy is checked until it is in place, so another open
/// never removes a copy that is about to replace the project file.
fn lock_upgrade(temp: &Path) -> Result<fs::File, StoreError> {
	let file = fs::File::open(temp)?;
	match file.try_lock() {
		Ok(()) => Ok(file),
		Err(fs::TryLockError::WouldBlock) => Err(StoreError::AlreadyOpen),
		Err(fs::TryLockError::Error(err)) => Err(err.into()),
	}
}

/// Runs only once this open holds the project file for writing.
fn remove_stale_upgrade(path: &Path) -> Result<(), StoreError> {
	let temp = upgrade_path(path);
	match fs::symlink_metadata(&temp) {
		Ok(meta) if meta.is_file() => {}
		Ok(_) => {
			return Err(StoreError::UpgradeFailed(format!(
				"{} is in the way of the upgraded copy",
				cryarchiver_engine::text::display_path(&temp)
			)));
		}
		Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(err) => return Err(err.into()),
	}

	let _held = lock_upgrade(&temp)?;
	fs::remove_file(&temp)?;
	Ok(())
}

/// Removes the upgrade copy only while it is still the file this upgrade created.
fn remove_if_ours(temp: &Path, ours: FileIdentity) {
	if file_id(temp).is_ok_and(|found| found == ours) {
		let _ = fs::remove_file(temp);
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameWindow {
	HoldingTheFile,
	AfterReleasingIt,
}

#[cfg(test)]
type RenameHook = Box<dyn FnMut(RenameWindow) -> bool>;

#[cfg(test)]
thread_local! {
	/// Runs right before each rename attempt of an upgrade on this thread; returning true fails that attempt.
	static BEFORE_UPGRADE_RENAME: std::cell::RefCell<Option<RenameHook>> = const { std::cell::RefCell::new(None) };
}

fn rename_into_place(
	temp: &Path,
	path: &Path,
	#[cfg_attr(not(test), expect(unused_variables))] window: RenameWindow,
) -> io::Result<()> {
	#[cfg(test)]
	if let Some(mut hook) = BEFORE_UPGRADE_RENAME.take() {
		let fail = hook(window);
		BEFORE_UPGRADE_RENAME.set(Some(hook));
		if fail {
			return Err(io::ErrorKind::PermissionDenied.into());
		}
	}

	retry_transient(|| fs::rename(temp, path))
}

/// Writes the upgraded project next to the old file and replaces it with one rename, once the copy is synced and read
/// back. A crash before the rename leaves the old file untouched; the next open removes the copy and starts again.
///
/// The old file stays open read-only until the copy is in place, which keeps every writer out of it. Where the file
/// system can't rename over an open file (FAT and exFAT on Windows), the rename is tried again after closing it;
/// the copy's lock still keeps other upgrades away, and a writer that opens the old file in that moment makes the
/// rename fail rather than lose its writes.
fn migrate(path: &Path) -> Result<(), StoreError> {
	let temp = upgrade_path(path);
	let old = ReadOnlyDatabase::open(path)?;
	match read_meta(&old)?.format {
		format if format > FORMAT_VERSION => return Err(StoreError::TooNew(format)),
		FORMAT_VERSION => return Ok(()),
		_ => {}
	}

	let file = create_new_file(&temp)?;
	let ours = match file_id(&temp) {
		Ok(identity) => identity,
		Err(err) => {
			drop(file);
			let _ = fs::remove_file(&temp);
			return Err(err.into());
		}
	};

	let checked = Database::builder()
		.create_file(file)
		.map_err(StoreError::from)
		.and_then(|new| write_migrated(&old, &new))
		.and_then(|()| Ok(fs::set_permissions(&temp, fs::metadata(path)?.permissions())?))
		.and_then(|()| Ok(fs::OpenOptions::new().write(true).open(&temp)?.sync_all()?))
		.and_then(|()| verify_migrated(&old, &temp))
		.and_then(|()| lock_upgrade(&temp));
	let held = match checked {
		Ok(held) => held,
		Err(err) => {
			remove_if_ours(&temp, ours);
			return Err(err);
		}
	};

	let renamed = match rename_into_place(&temp, path, RenameWindow::HoldingTheFile) {
		Err(_) if cfg!(any(windows, test)) => {
			drop(old);
			rename_into_place(&temp, path, RenameWindow::AfterReleasingIt)
		}
		renamed => renamed,
	};
	if let Err(err) = renamed {
		remove_if_ours(&temp, ours);
		drop(held);
		return Err(err.into());
	}

	// On Unix the rename is durable only once the folder that holds it is synced. It has happened either way, so a
	// share that can't sync a folder doesn't fail the open.
	#[cfg(unix)]
	let _ = fs::File::open(parent_folder(path)).and_then(|folder| folder.sync_all());
	drop(held);
	Ok(())
}

/// Format-1 values are plain `MessagePack`: they are packed and re-keyed, never decoded.
fn write_migrated(old: &ReadOnlyDatabase, new: &Database) -> Result<(), StoreError> {
	let codec = Codec::built_in();
	let mut folders = FolderIds::with_root();
	let read = old.begin_read()?;

	let txn = new.begin_write()?;
	{
		let mut meta = txn.open_table(META)?;
		for entry in read.open_table(META)?.iter()? {
			let (key, value) = entry?;
			meta.insert(key.value(), value.value())?;
		}
		meta.insert(FORMAT_KEY, rmp_serde::to_vec_named(&FORMAT_VERSION)?.as_slice())?;
		meta.insert(DICTIONARY_KEY, codec.dictionary.as_slice())?;

		let mut rows = txn.open_table(FOLDERS)?;
		for entry in read.open_table(FOLDERS)?.iter()? {
			let (key, value) = entry?;
			rows.insert(key.value(), codec.pack(value.value()).as_slice())?;
		}

		let mut files = txn.open_table(FILES)?;
		for entry in read.open_table(FILES)?.iter()? {
			let (key, value) = entry?;
			let key = folders.key_for(&RelPath::from_bytes(key.value().to_vec()))?;
			files.insert(key.as_slice(), codec.pack(value.value()).as_slice())?;
		}

		let mut logs = txn.open_table(LOGS)?;
		for entry in read.open_table(LOGS)?.iter()? {
			let (key, value) = entry?;
			let (path, attempt) = key.value();
			let key = folders.key_for(&RelPath::from_bytes(path.to_vec()))?;
			logs.insert((key.as_slice(), attempt), codec.pack(value.value()).as_slice())?;
		}

		let mut unknown = txn.open_table(UNKNOWN)?;
		for entry in read.open_table(UNKNOWN)?.iter()? {
			let (key, value) = entry?;
			let key = folders.key_for(&RelPath::from_bytes(key.value().to_vec()))?;
			unknown.insert(key.as_slice(), codec.pack(value.value()).as_slice())?;
		}

		let mut ids = txn.open_table(FOLDER_IDS)?;
		for (id, folder) in &folders.paths {
			ids.insert(*id, folder.as_bytes())?;
		}
	}
	txn.commit()?;
	Ok(())
}

/// Reads the upgraded copy back from its file: it must hold every entry of the old file, byte for byte once
/// unpacked, under the same paths, and nothing else.
fn verify_migrated(old: &ReadOnlyDatabase, upgraded: &Path) -> Result<(), StoreError> {
	let differs = || StoreError::UpgradeFailed("the upgraded copy didn't read back the same".into());
	let new = ReadOnlyDatabase::open(upgraded)?;
	if read_meta(&new)?.format != FORMAT_VERSION {
		return Err(differs());
	}

	let (codec, folders) = read_layout(&new)?;
	let (old, new) = (old.begin_read()?, new.begin_read()?);

	let new_meta = new.open_table(META)?;
	for entry in old.open_table(META)?.iter()? {
		let (key, value) = entry?;
		let kept = key.value() == FORMAT_KEY
			|| new_meta
				.get(key.value())?
				.is_some_and(|stored| stored.value() == value.value());
		if !kept {
			return Err(differs());
		}
	}

	let unpacked = |table: ReadOnlyTable<&'static [u8], &'static [u8]>, by_folder: bool| {
		let mut entries = HashMap::new();
		for entry in table.iter()? {
			let (key, value) = entry?;
			let path = if by_folder {
				folders.path_of(key.value())?
			} else {
				RelPath::from_bytes(key.value().to_vec())
			};
			entries.insert(path.as_bytes().to_vec(), codec.unpack(value.value())?);
		}
		Ok::<_, StoreError>(entries)
	};
	for (table, by_folder) in [(FILES, true), (FOLDERS, false), (UNKNOWN, true)] {
		let mut upgraded = unpacked(new.open_table(table)?, by_folder)?;
		for entry in old.open_table(table)?.iter()? {
			let (key, value) = entry?;
			if upgraded.remove(key.value()).as_deref() != Some(value.value()) {
				return Err(differs());
			}
		}
		if !upgraded.is_empty() {
			return Err(differs());
		}
	}

	let mut upgraded = HashMap::new();
	for entry in new.open_table(LOGS)?.iter()? {
		let (key, value) = entry?;
		let (path, attempt) = key.value();
		upgraded.insert(
			(folders.path_of(path)?.as_bytes().to_vec(), attempt),
			codec.unpack(value.value())?,
		);
	}
	for entry in old.open_table(LOGS)?.iter()? {
		let (key, value) = entry?;
		let (path, attempt) = key.value();
		if upgraded.remove(&(path.to_vec(), attempt)).as_deref() != Some(value.value()) {
			return Err(differs());
		}
	}
	if !upgraded.is_empty() {
		return Err(differs());
	}
	Ok(())
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
	Ok(rmp_serde::from_slice(bytes)?)
}

fn read_meta(db: &impl ReadableDatabase) -> Result<Meta, StoreError> {
	let txn = db.begin_read()?;
	let table = txn.open_table(META)?;
	let field = |key: &str| -> Result<Vec<u8>, StoreError> {
		table
			.get(key)?
			.map(|value| value.value().to_vec())
			.ok_or_else(|| StoreError::Damaged(format!("the {key} entry is missing")))
	};

	Ok(Meta {
		format: decode(&field(FORMAT_KEY)?)?,
		app_version: decode(&field(APP_VERSION_KEY)?)?,
		created: decode(&field(CREATED_KEY)?)?,
		settings: decode(&field(SETTINGS_KEY)?)?,
	})
}

/// Whether a project file is there to read. The peek and the recent projects list both decide with this, so they
/// agree on what is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
	File,
	/// Something other than a file, such as a folder, is at the path.
	NotAFile,
	/// The file is gone from a folder that is still there.
	Missing,
	/// The folder, drive or share that holds the file can't be reached now.
	Unavailable,
}

pub fn presence(path: &Path) -> Presence {
	match std::fs::metadata(path) {
		Ok(meta) if meta.is_file() => Presence::File,
		Ok(_) => Presence::NotAFile,
		Err(err) if err.kind() == io::ErrorKind::NotFound && parent_folder(path).is_dir() => Presence::Missing,
		Err(_) => Presence::Unavailable,
	}
}

fn parent_folder(path: &Path) -> &Path {
	match path.parent() {
		Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
		Some(parent) => parent,
		None => path,
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct Peeked {
	pub meta: Meta,
	/// The project wasn't closed cleanly; opening it recovers it.
	pub needs_recovery: bool,
}

/// A project that wasn't closed cleanly is read into memory and recovered there, up to this size; the file itself is
/// never written.
const RECOVERY_PEEK_LIMIT: u64 = 256 * 1024 * 1024;

/// Reads the meta table of a closed project without writing to it. The read-only handle holds a shared lock while
/// it reads and releases it before returning; an open that starts in that same moment fails with
/// [`StoreError::AlreadyOpen`], and may simply be tried again.
pub fn peek(path: &Path) -> Result<Peeked, StoreError> {
	match presence(path) {
		Presence::File => {}
		Presence::NotAFile => return Err(StoreError::NotAProjectFile),
		Presence::Missing => return Err(StoreError::Missing),
		Presence::Unavailable => return Err(StoreError::Unavailable),
	}

	peek_within(path, RECOVERY_PEEK_LIMIT)
}

/// The meta table of a project that isn't open, read without writing anything to the file, whatever its size: an open
/// decides its refusals on it before the file is opened for writing, which writes to it at once.
pub fn read_closed(path: &Path) -> Result<Meta, StoreError> {
	require_a_file(path)?;
	peek_within(path, u64::MAX).map(|peeked| peeked.meta)
}

fn peek_within(path: &Path, recovery_limit: u64) -> Result<Peeked, StoreError> {
	let (meta, needs_recovery) = match ReadOnlyDatabase::open(path) {
		Err(redb::DatabaseError::RepairAborted) => (read_unclean(path, recovery_limit)?, true),
		opened => (read_meta(&opened?)?, false),
	};
	if meta.format > FORMAT_VERSION {
		return Err(StoreError::TooNew(meta.format));
	}
	Ok(Peeked { meta, needs_recovery })
}

/// Lets redb recover the file under a shared lock with every write kept in memory, so the settings of a project that
/// wasn't closed cleanly can be read without opening it. The file is read once, only as far as the recovery reads it,
/// and never copied whole.
fn read_unclean(path: &Path, limit: u64) -> Result<Meta, StoreError> {
	use std::fs::TryLockError;

	let file = fs::File::open(path)?;
	let len = file.metadata()?.len();
	if len > limit {
		return Err(StoreError::NeedsRecovery);
	}

	match file.try_lock_shared() {
		Ok(()) => {}
		Err(TryLockError::WouldBlock) => return Err(StoreError::AlreadyOpen),
		Err(TryLockError::Error(err)) => return Err(err.into()),
	}

	let db = redb::Builder::new()
		.create_with_backend(Overlay::new(file, len))
		.map_err(|_| StoreError::NeedsRecovery)?;
	read_meta(&db)
}

const OVERLAY_PAGE_BYTES: usize = 4096;
const OVERLAY_PAGE: u64 = OVERLAY_PAGE_BYTES as u64;

/// A file seen through a layer of written pages: reads come from the file wherever nothing was written, and writes
/// never reach it.
#[derive(Debug)]
struct Overlay {
	file: fs::File,
	state: std::sync::Mutex<OverlayState>,
}

#[derive(Debug)]
struct OverlayState {
	len: u64,
	/// Below this the file's own bytes show through; above it, unwritten bytes are zeros, as after a shrink and regrow.
	file_len: u64,
	pages: std::collections::BTreeMap<u64, Vec<u8>>,
}

impl Overlay {
	fn new(file: fs::File, len: u64) -> Self {
		Self {
			file,
			state: std::sync::Mutex::new(OverlayState {
				len,
				file_len: len,
				pages: std::collections::BTreeMap::new(),
			}),
		}
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, OverlayState> {
		self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Fills `out` with the file's bytes from `offset`, and zeros past `file_len`.
	fn read_file(&self, offset: u64, out: &mut [u8], file_len: u64) -> io::Result<()> {
		let readable = usize::try_from(file_len.saturating_sub(offset))
			.unwrap_or(usize::MAX)
			.min(out.len());
		out[readable..].fill(0);
		read_exact_at(&self.file, &mut out[..readable], offset)
	}
}

impl redb::StorageBackend for Overlay {
	fn len(&self) -> io::Result<u64> {
		Ok(self.lock().len)
	}

	fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
		let state = self.lock();
		if offset.saturating_add(out.len() as u64) > state.len {
			return Err(io::ErrorKind::UnexpectedEof.into());
		}

		let mut done = 0;
		while done < out.len() {
			let at = offset + done as u64;
			let (page, within) = page_of(at);
			let take = (OVERLAY_PAGE_BYTES - within).min(out.len() - done);
			let chunk = &mut out[done..done + take];
			match state.pages.get(&page) {
				Some(bytes) => chunk.copy_from_slice(&bytes[within..within + take]),
				None => self.read_file(at, chunk, state.file_len)?,
			}
			done += take;
		}
		Ok(())
	}

	fn set_len(&self, len: u64) -> io::Result<()> {
		let mut state = self.lock();
		if len < state.len {
			state.pages.retain(|page, _| page * OVERLAY_PAGE < len);
			let (last, kept) = page_of(len);
			if let Some(bytes) = state.pages.get_mut(&last) {
				bytes[kept..].fill(0);
			}
			state.file_len = state.file_len.min(len);
		}
		state.len = len;
		Ok(())
	}

	fn sync_data(&self) -> io::Result<()> {
		Ok(())
	}

	fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
		let mut state = self.lock();
		let mut done = 0;
		while done < data.len() {
			let at = offset + done as u64;
			let (page, within) = page_of(at);
			let take = (OVERLAY_PAGE_BYTES - within).min(data.len() - done);
			if !state.pages.contains_key(&page) {
				let mut bytes = vec![0; OVERLAY_PAGE_BYTES];
				self.read_file(page * OVERLAY_PAGE, &mut bytes, state.file_len)?;
				state.pages.insert(page, bytes);
			}

			let bytes = state.pages.get_mut(&page).expect("inserted above");
			bytes[within..within + take].copy_from_slice(&data[done..done + take]);
			done += take;
		}
		state.len = state.len.max(offset + data.len() as u64);
		Ok(())
	}
}

/// The page that holds byte `at`, and where in it the byte is.
fn page_of(at: u64) -> (u64, usize) {
	let within = usize::try_from(at % OVERLAY_PAGE).expect("smaller than a page");
	(at / OVERLAY_PAGE, within)
}

#[cfg(windows)]
fn read_exact_at(file: &fs::File, mut out: &mut [u8], mut offset: u64) -> io::Result<()> {
	use std::os::windows::fs::FileExt;

	while !out.is_empty() {
		match file.seek_read(out, offset)? {
			0 => return Err(io::ErrorKind::UnexpectedEof.into()),
			read => {
				out = &mut out[read..];
				offset += read as u64;
			}
		}
	}
	Ok(())
}

#[cfg(unix)]
fn read_exact_at(file: &fs::File, out: &mut [u8], offset: u64) -> io::Result<()> {
	use std::os::unix::fs::FileExt;

	file.read_exact_at(out, offset)
}

/// Prints every table as readable text, for debugging (`cryarchiver project dump`).
pub fn dump(path: &Path, out: &mut dyn Write) -> Result<(), StoreError> {
	let db = ReadOnlyDatabase::open(path)?;
	let meta = read_meta(&db)?;
	writeln!(out, "[meta]")?;
	writeln!(out, "format = {}", meta.format)?;
	writeln!(out, "app_version = {}", meta.app_version)?;
	writeln!(out, "created = {}", meta.created)?;
	writeln!(out, "settings = {:?}", meta.settings)?;
	if meta.format < FORMAT_VERSION {
		writeln!(out, "\n(format {}: open the project once to upgrade it)", meta.format)?;
		return Ok(());
	}

	let (codec, folders) = read_layout(&db)?;
	let txn = db.begin_read()?;
	writeln!(out, "\n[files]")?;
	for entry in txn.open_table(FILES)?.iter()? {
		let (key, value) = entry?;
		let record: FileRecord = decode(&codec.unpack(value.value())?)?;
		writeln!(out, "{:?} = {record:?}", folders.path_of(key.value())?)?;
	}

	writeln!(out, "\n[folders]")?;
	for entry in txn.open_table(FOLDERS)?.iter()? {
		let (key, value) = entry?;
		let record: FolderRecord = decode(&codec.unpack(value.value())?)?;
		writeln!(out, "{:?} = {record:?}", RelPath::from_bytes(key.value().to_vec()))?;
	}

	writeln!(out, "\n[logs]")?;
	for entry in txn.open_table(LOGS)?.iter()? {
		let (key, value) = entry?;
		let (path, attempt) = key.value();
		let log: FailureLog = decode(&codec.unpack(value.value())?)?;
		writeln!(out, "{:?} attempt {attempt}: {}", folders.path_of(path)?, log.error)?;
		for line in &log.log_tail {
			writeln!(out, "    {line}")?;
		}
	}

	writeln!(out, "\n[unknown]")?;
	for entry in txn.open_table(UNKNOWN)?.iter()? {
		let (key, value) = entry?;
		let record: UnknownRecord = decode(&codec.unpack(value.value())?)?;
		writeln!(out, "{:?} = {record:?}", folders.path_of(key.value())?)?;
	}

	writeln!(out, "\n[quality]")?;
	for entry in txn.open_table(META)?.iter()? {
		let (key, value) = entry?;
		if let Some(quality_key) = key.value().strip_prefix(QUALITY_PREFIX) {
			match decode::<GroupStats>(value.value()) {
				Ok(stats) => writeln!(out, "{quality_key} = {stats:?}")?,
				Err(err) => writeln!(out, "{quality_key} = unreadable ({err})")?,
			}
		}
	}
	Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
	use std::ffi::OsStr;

	use cryarchiver_engine::quality::memory::Offset;

	use super::*;

	fn path(name: &str) -> RelPath {
		RelPath::root().join(OsStr::new(name))
	}

	fn project(dir: &Path) -> std::path::PathBuf {
		dir.join("archive.cryarchive")
	}

	const GOOD_PHOTO: &str = "image/jpeg/photo/opaque/8bit/mp12|ravif-0.13.0|p6|t85.0";

	fn committing() -> FileRecord {
		FileRecord {
			state: FileState::Committing {
				attempt: 1,
				output: Output {
					how: How::Copied(CopyReason::NotMedia),
					path: path("a.txt"),
					size: 3,
					hash: Some(Hash([2; 32])),
				},
			},
			..FileRecord::discovered(3, 5)
		}
	}

	#[test]
	fn creates_and_reopens_with_the_settings() {
		let dir = tempfile::tempdir().unwrap();
		let settings = Settings {
			watch: true,
			..Settings::new(Path::new("src"), Path::new("dst"))
		};

		drop(Store::create(&project(dir.path()), &settings).unwrap());
		let (_, meta, loaded) = Store::open(&project(dir.path())).unwrap();

		assert_eq!(meta.format, FORMAT_VERSION);
		assert_eq!(meta.app_version, cryarchiver_engine::VERSION);
		assert_eq!(meta.settings, settings);
		assert!(loaded.files.is_empty());
	}

	#[test]
	fn creating_over_an_existing_file_is_refused() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(project(dir.path()), b"").unwrap();

		assert!(matches!(
			Store::create(&project(dir.path()), &Settings::default()),
			Err(StoreError::Exists(_))
		));
	}

	#[test]
	fn a_second_open_is_refused_while_the_first_holds_the_lock() {
		let dir = tempfile::tempdir().unwrap();
		let _first = Store::create(&project(dir.path()), &Settings::default()).unwrap();

		assert!(matches!(
			Store::open(&project(dir.path())),
			Err(StoreError::AlreadyOpen)
		));
	}

	#[test]
	fn batched_changes_are_lost_without_a_flush_but_intents_are_durable() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();

		store
			.set_file(&path("lost.txt"), &FileRecord::discovered(1, 1))
			.unwrap();
		drop(store);
		let (mut store, _, loaded) = Store::open(&project(dir.path())).unwrap();
		assert!(loaded.files.is_empty());

		store
			.set_file(&path("batched.txt"), &FileRecord::discovered(1, 1))
			.unwrap();
		store.write_intent(&path("a.txt"), &committing()).unwrap();
		drop(store);
		let (_, _, loaded) = Store::open(&project(dir.path())).unwrap();

		let names: Vec<_> = loaded.files.iter().map(|(key, _)| key.display()).collect();
		assert_eq!(names, ["a.txt", "batched.txt"]);
		assert_eq!(loaded.files[0].1, committing());
	}

	#[test]
	fn batches_flush_after_1000_changes_or_250_ms() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();

		store.set_file(&path("a"), &FileRecord::default()).unwrap();
		assert!(!store.flush_if_due(Instant::now()).unwrap());
		assert!(store.flush_if_due(Instant::now() + BATCH_INTERVAL).unwrap());
		assert_eq!(store.pending_changes(), 0);

		for index in 0..BATCH_CHANGES {
			store
				.set_file(&path(&index.to_string()), &FileRecord::default())
				.unwrap();
		}
		assert!(store.flush_if_due(Instant::now()).unwrap());
	}

	#[test]
	fn logs_are_kept_per_attempt() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		let log = |error: &str| FailureLog {
			error: error.into(),
			log_tail: vec!["line".into()],
		};

		store.set_log(&path("a.mkv"), 1, &log("first")).unwrap();
		assert_eq!(store.read_log(&path("a.mkv"), 1).unwrap(), Some(log("first")));
		store.set_log(&path("a.mkv"), 2, &log("second")).unwrap();
		store.flush().unwrap();
		drop(store);

		let (store, _, loaded) = Store::open(&project(dir.path())).unwrap();
		assert_eq!(loaded.last_attempts.get(&path("a.mkv")), Some(&2));
		assert_eq!(store.read_log(&path("a.mkv"), 1).unwrap(), Some(log("first")));
		assert_eq!(store.read_log(&path("a.mkv"), 3).unwrap(), None);
	}

	#[test]
	fn a_newer_format_is_refused() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store
			.put(Table::Meta, FORMAT_KEY.as_bytes().to_vec(), &(FORMAT_VERSION + 1))
			.unwrap();
		store.flush().unwrap();
		drop(store);

		assert!(matches!(
			Store::open(&project(dir.path())),
			Err(StoreError::TooNew(format)) if format == FORMAT_VERSION + 1
		));
		let peeked = peek(&project(dir.path()));
		assert!(
			matches!(peeked, Err(StoreError::TooNew(format)) if format == FORMAT_VERSION + 1),
			"{peeked:?}"
		);
		assert!(peeked.unwrap_err().to_string().contains("made by a newer CryArchiver"));
	}

	#[test]
	fn peek_reads_the_settings_and_leaves_the_project_openable() {
		let dir = tempfile::tempdir().unwrap();
		let settings = Settings::new(Path::new("src"), Path::new("dst"));
		drop(Store::create(&project(dir.path()), &settings).unwrap());

		let peeked = peek(&project(dir.path())).unwrap();
		assert_eq!(peeked.meta.settings, settings);
		assert!(!peeked.needs_recovery);
		let opened = Store::open(&project(dir.path())).unwrap();

		assert!(matches!(peek(&project(dir.path())), Err(StoreError::AlreadyOpen)));
		drop(opened);
		assert!(matches!(
			peek(&dir.path().join("missing.cryarchive")),
			Err(StoreError::Missing)
		));
	}

	#[test]
	fn a_file_is_missing_only_while_its_folder_is_there() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::create_dir(dir.path().join("folder.cryarchive")).unwrap();
		let offline = dir.path().join("gone").join("a.cryarchive");

		assert_eq!(presence(&dir.path().join("a.cryarchive")), Presence::Missing);
		assert_eq!(presence(&offline), Presence::Unavailable);
		assert_eq!(presence(&dir.path().join("folder.cryarchive")), Presence::NotAFile);
		assert!(matches!(peek(&offline), Err(StoreError::Unavailable)));
		assert!(matches!(
			peek(&dir.path().join("folder.cryarchive")),
			Err(StoreError::NotAProjectFile)
		));
		assert_eq!(
			StoreError::Unavailable.to_string(),
			"unavailable (drive or share offline)"
		);
	}

	/// Shares one in-memory file between the database that writes it and the test that copies it out.
	#[derive(Debug, Clone)]
	struct SharedMemory(std::sync::Arc<redb::backends::InMemoryBackend>);

	impl redb::StorageBackend for SharedMemory {
		fn len(&self) -> Result<u64, io::Error> {
			self.0.len()
		}

		fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), io::Error> {
			self.0.read(offset, out)
		}

		fn set_len(&self, len: u64) -> Result<(), io::Error> {
			self.0.set_len(len)
		}

		fn sync_data(&self) -> Result<(), io::Error> {
			self.0.sync_data()
		}

		fn write(&self, offset: u64, data: &[u8]) -> Result<(), io::Error> {
			self.0.write(offset, data)
		}
	}

	/// Rewrites a closed project file as a crash leaves it: a transaction committed, the file never closed.
	pub(crate) fn leave_unclean(file: &Path) {
		use redb::StorageBackend;

		let bytes = std::fs::read(file).unwrap();
		let memory = SharedMemory(std::sync::Arc::default());
		memory.set_len(u64::try_from(bytes.len()).unwrap()).unwrap();
		memory.write(0, &bytes).unwrap();

		let db = redb::Builder::new().create_with_backend(memory.clone()).unwrap();
		let txn = db.begin_write().unwrap();
		{
			let mut meta = txn.open_table(META).unwrap();
			let settings = meta.get(SETTINGS_KEY).unwrap().unwrap().value().to_vec();
			meta.insert(SETTINGS_KEY, settings.as_slice()).unwrap();
		}
		txn.commit().unwrap();

		let mut unclean = vec![0; usize::try_from(memory.len().unwrap()).unwrap()];
		memory.read(0, &mut unclean).unwrap();
		std::mem::forget(db);
		std::fs::write(file, &unclean).unwrap();
	}

	#[test]
	fn a_project_that_was_not_closed_cleanly_still_shows_its_settings() {
		use redb::StorageBackend;

		let dir = tempfile::tempdir().unwrap();
		let settings = Settings {
			delete_originals: true,
			..Settings::new(Path::new("src"), Path::new("dst"))
		};
		let memory = SharedMemory(std::sync::Arc::default());
		let db = redb::Builder::new().create_with_backend(memory.clone()).unwrap();
		write_new(&db, &settings, &Codec::built_in()).unwrap();
		let mut bytes = vec![0; usize::try_from(memory.len().unwrap()).unwrap()];
		memory.read(0, &mut bytes).unwrap();
		std::mem::forget(db);
		std::fs::write(project(dir.path()), &bytes).unwrap();
		assert!(matches!(
			ReadOnlyDatabase::open(project(dir.path())),
			Err(redb::DatabaseError::RepairAborted)
		));

		let peeked = peek(&project(dir.path())).unwrap();

		assert!(peeked.needs_recovery);
		assert_eq!(peeked.meta.settings, settings);
		assert_eq!(std::fs::read(project(dir.path())).unwrap(), bytes);
	}

	#[test]
	fn the_recovery_overlay_reads_the_file_and_keeps_its_writes_to_itself() {
		use redb::StorageBackend;

		let dir = tempfile::tempdir().unwrap();
		let original: Vec<u8> = (0..10_000_u32).map(|index| (index % 251) as u8).collect();
		std::fs::write(project(dir.path()), &original).unwrap();
		let overlay = Overlay::new(std::fs::File::open(project(dir.path())).unwrap(), original.len() as u64);
		let read = |offset: u64, len: usize| {
			let mut out = vec![0; len];
			overlay.read(offset, &mut out).unwrap();
			out
		};

		overlay.write(4090, &[7; 12]).unwrap();

		assert_eq!(read(4080, 30)[..10], original[4080..4090]);
		assert_eq!(read(4080, 30)[10..22], [7; 12]);
		assert_eq!(read(4080, 30)[22..], original[4102..4110]);

		overlay.set_len(5000).unwrap();
		overlay.set_len(12_000).unwrap();

		assert_eq!(read(4990, 10), original[4990..5000]);
		assert!(read(5000, 7000).iter().all(|byte| *byte == 0));
		assert!(overlay.read(11_999, &mut [0; 2]).is_err());
		assert_eq!(std::fs::read(project(dir.path())).unwrap(), original);
	}

	#[test]
	fn peek_refuses_a_file_that_is_not_a_project() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(project(dir.path()), b"not a database").unwrap();

		assert!(peek(&project(dir.path())).is_err());
	}

	#[test]
	fn dump_prints_every_table() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store.write_intent(&path("a.txt"), &committing()).unwrap();
		store.set_folder(&RelPath::root(), &FolderRecord::default()).unwrap();
		store.flush().unwrap();
		drop(store);

		let mut out = Vec::new();
		dump(&project(dir.path()), &mut out).unwrap();
		let text = String::from_utf8(out).unwrap();

		assert!(text.contains("format = 2"), "{text}");
		assert!(text.contains("\"a.txt\" = FileRecord"), "{text}");
		assert!(text.contains("Committing"), "{text}");
		assert!(text.contains("[folders]\n\"\" = FolderRecord"), "{text}");
	}

	#[test]
	fn learned_quality_is_kept_in_the_meta_table_and_dumped() {
		let dir = tempfile::tempdir().unwrap();
		let key = "video/h264/1080/fps31/sdr8/progressive/clean/opaque|svt-av1-4.2|p5|t95.0/90.0";
		let stats = GroupStats {
			setting: 29.5,
			slope: Some(0.4),
			bpp: Some(0.03),
			offset: Some(Offset { mean: -0.4, p5: -2.7 }),
			observations: 2,
			without_grain: false,
		};
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store.set_quality(key, &stats).unwrap();
		store.flush().unwrap();
		drop(store);

		let (store, meta, loaded) = Store::open(&project(dir.path())).unwrap();
		drop(store);
		let mut out = Vec::new();
		dump(&project(dir.path()), &mut out).unwrap();

		assert_eq!(loaded.quality, vec![(key.to_owned(), stats)]);
		assert_eq!(meta.settings, Settings::default());
		assert!(
			String::from_utf8(out)
				.unwrap()
				.contains(&format!("[quality]\n{key} = GroupStats")),
			"the dump lists learned quality"
		);
	}

	#[test]
	fn a_forgotten_group_keeps_its_sample_offset_across_a_reopen() {
		let dir = tempfile::tempdir().unwrap();
		let key = "video/h264/1080/fps31/sdr8/progressive/clean/opaque|svt-av1-4.2.0|p5|t93.0/89.0";
		let forgotten = GroupStats {
			setting: 29.5,
			slope: Some(0.4),
			bpp: Some(0.03),
			offset: Some(Offset { mean: -0.6, p5: -2.1 }),
			observations: 3,
			without_grain: false,
		}
		.forgotten()
		.unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store.set_quality(key, &forgotten).unwrap();
		store.flush().unwrap();
		drop(store);

		let (store, _, loaded) = Store::open(&project(dir.path())).unwrap();
		drop(store);

		assert_eq!(loaded.quality, vec![(key.to_owned(), forgotten)]);
		assert!(!loaded.quality[0].1.learned());
		assert!(loaded.ignored_quality.is_empty(), "{:?}", loaded.ignored_quality);
	}

	#[test]
	fn a_quality_entry_stored_before_sample_offsets_reads_without_one() {
		#[derive(Serialize)]
		struct Earlier {
			setting: f64,
			slope: Option<f64>,
			bpp: Option<f64>,
			observations: u32,
			without_grain: bool,
		}

		let dir = tempfile::tempdir().unwrap();
		let key = "video/h264/1080/fps31/sdr8/progressive/clean/opaque|svt-av1-4.2.0|p5|t93.0/89.0";
		let earlier = Earlier {
			setting: 29.5,
			slope: Some(0.4),
			bpp: None,
			observations: 2,
			without_grain: false,
		};
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store.push(Change {
			table: Table::Meta,
			key: format!("{QUALITY_PREFIX}{key}").into_bytes(),
			value: Some(rmp_serde::to_vec_named(&earlier).unwrap()),
		});
		store.flush().unwrap();
		drop(store);

		let (store, _, loaded) = Store::open(&project(dir.path())).unwrap();
		drop(store);

		assert_eq!(
			loaded.quality,
			vec![(
				key.to_owned(),
				GroupStats {
					setting: 29.5,
					slope: Some(0.4),
					bpp: None,
					offset: None,
					observations: 2,
					without_grain: false,
				}
			)]
		);
		assert!(loaded.ignored_quality.is_empty(), "{:?}", loaded.ignored_quality);
	}

	#[test]
	fn an_unreadable_quality_entry_is_skipped_and_the_project_still_opens() {
		let dir = tempfile::tempdir().unwrap();
		let stats = GroupStats {
			setting: 88.0,
			slope: None,
			bpp: None,
			offset: None,
			observations: 1,
			without_grain: false,
		};
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store.set_quality(GOOD_PHOTO, &stats).unwrap();
		store.push(Change {
			table: Table::Meta,
			key: format!("{QUALITY_PREFIX}x").into_bytes(),
			value: Some(vec![0xC1, 0xFF, 0x00]),
		});
		store.flush().unwrap();
		drop(store);

		let (store, _, loaded) = Store::open(&project(dir.path())).unwrap();
		drop(store);
		let mut out = Vec::new();
		dump(&project(dir.path()), &mut out).unwrap();
		let text = String::from_utf8(out).unwrap();

		assert_eq!(loaded.quality, vec![(GOOD_PHOTO.to_owned(), stats)]);
		assert_eq!(loaded.ignored_quality.len(), 1, "{:?}", loaded.ignored_quality);
		assert!(
			loaded.ignored_quality[0].contains("for x was ignored"),
			"{:?}",
			loaded.ignored_quality
		);
		assert!(text.contains(&format!("{GOOD_PHOTO} = GroupStats")), "{text}");
		assert!(text.contains("x = unreadable"), "{text}");
	}

	fn nested(path: &str) -> RelPath {
		RelPath::from_bytes(path.as_bytes().to_vec())
	}

	#[test]
	fn records_round_trip_compressed_under_folder_ids() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		let files: Vec<(RelPath, FileRecord)> = (0..300)
			.map(|index| {
				(
					nested(&format!("Trip {}/Day {}/{index}.jpg", index / 100, index / 10)),
					committing(),
				)
			})
			.collect();
		for (key, record) in &files {
			store.set_file(key, record).unwrap();
		}
		store
			.set_unknown(&nested("Trip 0/stray.bin"), &UnknownRecord::default())
			.unwrap();
		store.flush().unwrap();
		let raw: u64 = files
			.iter()
			.map(|(key, record)| {
				u64::try_from(key.as_bytes().len() + rmp_serde::to_vec_named(record).unwrap().len()).unwrap()
			})
			.sum();
		let stored = store
			.table_sizes()
			.unwrap()
			.into_iter()
			.find(|(table, _)| *table == "files")
			.unwrap()
			.1
			.stored_bytes;
		drop(store);

		let (_, _, loaded) = Store::open(&project(dir.path())).unwrap();

		let mut read = loaded.files;
		read.sort_by(|a, b| a.0.cmp(&b.0));
		let mut written = files;
		written.sort_by(|a, b| a.0.cmp(&b.0));
		assert_eq!(read, written);
		assert_eq!(loaded.unknown.len(), 1);
		assert!(stored * 10 < raw * 9, "{stored} stored bytes for {raw} raw bytes");
	}

	#[test]
	fn logs_keep_the_last_lines_of_the_last_five_attempts() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		let long = FailureLog {
			error: format!("encode failed: {}", "x".repeat(1000)),
			log_tail: (0..500).map(|line| format!("{line} {}", "y".repeat(1000))).collect(),
		};

		for attempt in 1..=7 {
			store.set_log(&path("a.mkv"), attempt, &long).unwrap();
		}
		store.flush().unwrap();

		assert_eq!(store.read_log(&path("a.mkv"), 2).unwrap(), None);
		let kept = store.read_log(&path("a.mkv"), 3).unwrap().unwrap();
		assert_eq!(kept.log_tail.len(), LOG_TAIL_LINES);
		assert!(kept.log_tail[0].starts_with("400 "), "{}", &kept.log_tail[0][..10]);
		assert!(kept.log_tail.iter().all(|line| line.chars().count() <= LOG_LINE_CHARS));
		assert_eq!(kept.error.chars().count(), LOG_LINE_CHARS);
	}

	#[test]
	fn pruning_keeps_the_newest_log_of_every_failing_file() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		let log = |error: &str| FailureLog {
			error: error.into(),
			log_tail: vec!["line".into()],
		};
		store.set_log(&path("fixed.mkv"), 1, &log("fixed")).unwrap();
		store.set_log(&path("failing.mkv"), 1, &log("old")).unwrap();
		store.set_log(&path("failing.mkv"), 2, &log("new")).unwrap();
		let failing = HashSet::from([path("failing.mkv")]);

		let removed = store.prune_logs(&failing, 1).unwrap();

		assert_eq!(removed, 2);
		assert_eq!(store.read_log(&path("fixed.mkv"), 1).unwrap(), None);
		assert_eq!(store.read_log(&path("failing.mkv"), 1).unwrap(), None);
		assert_eq!(store.read_log(&path("failing.mkv"), 2).unwrap(), Some(log("new")));
	}

	#[test]
	fn compaction_gives_back_the_space_of_removed_records() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		for index in 0..5_000 {
			store.set_file(&path(&format!("{index}.txt")), &committing()).unwrap();
		}
		store.flush().unwrap();
		for index in 0..5_000 {
			store.remove_file(&path(&format!("{index}.txt")));
		}
		store.flush().unwrap();
		let before = std::fs::metadata(project(dir.path())).unwrap().len();

		assert!(store.compact().unwrap());
		assert!(std::fs::metadata(project(dir.path())).unwrap().len() < before);
	}

	const OFFSET_GROUP: &str = "video/h264/1080/fps31/sdr8/progressive/clean/opaque|svt-av1-4.2.0|p5|t93.0/89.0";
	const OFFSET_ONLY_GROUP: &str = "video/hevc/1080/fps31/sdr8/progressive/clean/opaque|svt-av1-4.2.0|p5|t93.0/89.0";

	fn offset_stats() -> GroupStats {
		GroupStats {
			setting: 29.5,
			slope: Some(0.4),
			bpp: Some(0.03),
			offset: Some(Offset { mean: -0.6, p5: -2.1 }),
			observations: 3,
			without_grain: false,
		}
	}

	fn offset_only_stats() -> GroupStats {
		let stats = offset_stats().forgotten().unwrap();
		assert_eq!(stats.observations, 0);
		stats
	}

	fn write_format_1(path: &Path, files: &[(RelPath, FileRecord)], log: &(RelPath, u32, FailureLog)) {
		let db = Database::create(path).unwrap();
		let txn = db.begin_write().unwrap();
		{
			let mut meta = txn.open_table(META).unwrap();
			meta.insert(FORMAT_KEY, rmp_serde::to_vec_named(&1u32).unwrap().as_slice())
				.unwrap();
			meta.insert(APP_VERSION_KEY, rmp_serde::to_vec_named("0.1.0").unwrap().as_slice())
				.unwrap();
			meta.insert(CREATED_KEY, rmp_serde::to_vec_named(&1u64).unwrap().as_slice())
				.unwrap();
			meta.insert(
				SETTINGS_KEY,
				rmp_serde::to_vec_named(&Settings::default()).unwrap().as_slice(),
			)
			.unwrap();
			meta.insert(
				format!("{QUALITY_PREFIX}{OFFSET_GROUP}").as_str(),
				rmp_serde::to_vec_named(&offset_stats()).unwrap().as_slice(),
			)
			.unwrap();
			meta.insert(
				format!("{QUALITY_PREFIX}{OFFSET_ONLY_GROUP}").as_str(),
				rmp_serde::to_vec_named(&offset_only_stats()).unwrap().as_slice(),
			)
			.unwrap();

			let mut table = txn.open_table(FILES).unwrap();
			for (key, record) in files {
				table
					.insert(key.as_bytes(), rmp_serde::to_vec_named(record).unwrap().as_slice())
					.unwrap();
			}
			txn.open_table(FOLDERS).unwrap();
			txn.open_table(LOGS)
				.unwrap()
				.insert(
					(log.0.as_bytes(), log.1),
					rmp_serde::to_vec_named(&log.2).unwrap().as_slice(),
				)
				.unwrap();
			txn.open_table(UNKNOWN).unwrap();
		}
		txn.commit().unwrap();
	}

	#[test]
	fn a_format_1_project_is_upgraded_in_place_and_keeps_every_record() {
		let dir = tempfile::tempdir().unwrap();
		let mut files = vec![
			(path("a.jpg"), committing()),
			(nested("Trip/b.flac"), FileRecord::discovered(9, 9)),
		];
		let log = (
			path("a.jpg"),
			2,
			FailureLog {
				error: "first".into(),
				log_tail: vec!["line".into()],
			},
		);
		write_format_1(&project(dir.path()), &files, &log);
		std::fs::write(upgrade_path(&project(dir.path())), b"left over from a crash").unwrap();
		let mut dumped = Vec::new();
		dump(&project(dir.path()), &mut dumped).unwrap();
		let dumped = String::from_utf8(dumped).unwrap();
		assert!(dumped.contains("format = 1"), "{dumped}");
		assert!(
			dumped.contains("(format 1: open the project once to upgrade it)"),
			"{dumped}"
		);

		let (store, meta, loaded) = Store::open(&project(dir.path())).unwrap();

		assert_eq!(meta.format, FORMAT_VERSION);
		let mut read = loaded.files;
		read.sort_by(|a, b| a.0.cmp(&b.0));
		files.sort_by(|a, b| a.0.cmp(&b.0));
		assert_eq!(read, files);
		assert_eq!(loaded.last_attempts.get(&path("a.jpg")), Some(&2));
		assert_eq!(
			loaded.quality,
			vec![
				(OFFSET_GROUP.to_owned(), offset_stats()),
				(OFFSET_ONLY_GROUP.to_owned(), offset_only_stats())
			]
		);
		assert!(loaded.ignored_quality.is_empty(), "{:?}", loaded.ignored_quality);
		assert_eq!(store.read_log(&path("a.jpg"), 2).unwrap(), Some(log.2));
		assert!(!upgrade_path(&project(dir.path())).exists());
	}

	#[test]
	fn an_upgrade_that_cannot_start_leaves_the_format_1_project_as_it_was() {
		let dir = tempfile::tempdir().unwrap();
		let log = (path("a.jpg"), 1, FailureLog::default());
		write_format_1(&project(dir.path()), &[(path("a.jpg"), committing())], &log);
		std::fs::create_dir(upgrade_path(&project(dir.path()))).unwrap();

		let opened = Store::open(&project(dir.path()));

		let Err(refused @ StoreError::UpgradeFailed(_)) = opened else {
			panic!("{:?}", opened.err());
		};
		assert!(refused.to_string().contains("in the way"), "{refused}");
		assert!(refused.to_string().contains("left as it was"), "{refused}");
		assert!(!refused.to_string().contains("damaged"), "{refused}");
		assert_eq!(read_closed(&project(dir.path())).unwrap().format, 1);
		std::fs::remove_dir(upgrade_path(&project(dir.path()))).unwrap();
		let (_, meta, loaded) = Store::open(&project(dir.path())).unwrap();
		assert_eq!(meta.format, FORMAT_VERSION);
		assert_eq!(loaded.files, vec![(path("a.jpg"), committing())]);
	}

	#[test]
	fn an_upgrade_finds_a_project_another_open_already_upgraded_and_leaves_it() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store.write_intent(&path("a.txt"), &committing()).unwrap();
		drop(store);
		let before = std::fs::read(project(dir.path())).unwrap();

		migrate(&project(dir.path())).unwrap();

		assert_eq!(std::fs::read(project(dir.path())).unwrap(), before);
		assert!(!upgrade_path(&project(dir.path())).exists());
	}

	fn format_1_project(dir: &Path) -> Vec<(RelPath, FileRecord)> {
		let mut files = vec![
			(path("a.jpg"), committing()),
			(nested("Trip/b.flac"), FileRecord::discovered(9, 9)),
		];
		write_format_1(&project(dir), &files, &(path("a.jpg"), 1, FailureLog::default()));
		files.sort_by(|a, b| a.0.cmp(&b.0));
		files
	}

	fn sorted_files(loaded: Loaded) -> Vec<(RelPath, FileRecord)> {
		let mut files = loaded.files;
		files.sort_by(|a, b| a.0.cmp(&b.0));
		files
	}

	/// What a second open of the project, made right before a rename attempt of the upgrade, found.
	#[derive(Debug, PartialEq, Eq)]
	struct Intruder {
		window: RenameWindow,
		refused_as_open: bool,
		copy_still_there: bool,
	}

	type Opened = Result<(Store, Meta, Loaded), StoreError>;

	/// Opens the project with a second open made before each rename attempt; `fail` decides which attempts fail.
	fn open_with_an_intruder(file: &Path, fail: fn(RenameWindow) -> bool) -> (Opened, Vec<Intruder>) {
		let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
		let (hook_seen, hook_file) = (std::rc::Rc::clone(&seen), file.to_path_buf());
		BEFORE_UPGRADE_RENAME.set(Some(Box::new(move |window| {
			let second = Store::open(&hook_file);
			hook_seen.borrow_mut().push(Intruder {
				window,
				refused_as_open: matches!(second, Err(StoreError::AlreadyOpen)),
				copy_still_there: upgrade_path(&hook_file).is_file(),
			});
			fail(window)
		})));

		let opened = Store::open(file);
		BEFORE_UPGRADE_RENAME.set(None);
		let seen = seen.take();
		(opened, seen)
	}

	#[test]
	fn a_second_open_while_the_upgraded_copy_moves_in_is_refused_and_changes_nothing() {
		let dir = tempfile::tempdir().unwrap();
		let files = format_1_project(dir.path());

		let (opened, seen) = open_with_an_intruder(&project(dir.path()), |_| false);

		let (store, meta, loaded) = opened.unwrap();
		assert_eq!(
			seen,
			vec![Intruder {
				window: RenameWindow::HoldingTheFile,
				refused_as_open: true,
				copy_still_there: true,
			}]
		);
		assert_eq!(meta.format, FORMAT_VERSION);
		assert_eq!(sorted_files(loaded), files);
		assert!(!upgrade_path(&project(dir.path())).exists());
		drop(store);
		assert_eq!(sorted_files(Store::open(&project(dir.path())).unwrap().2), files);
	}

	#[test]
	fn a_second_open_after_the_old_file_is_released_leaves_the_checked_copy_alone() {
		let dir = tempfile::tempdir().unwrap();
		let files = format_1_project(dir.path());

		let (opened, seen) =
			open_with_an_intruder(&project(dir.path()), |window| window == RenameWindow::HoldingTheFile);

		let (store, meta, loaded) = opened.unwrap();
		assert_eq!(
			seen,
			vec![
				Intruder {
					window: RenameWindow::HoldingTheFile,
					refused_as_open: true,
					copy_still_there: true,
				},
				Intruder {
					window: RenameWindow::AfterReleasingIt,
					refused_as_open: true,
					copy_still_there: true,
				},
			]
		);
		assert_eq!(meta.format, FORMAT_VERSION);
		assert_eq!(sorted_files(loaded), files);
		drop(store);
		assert_eq!(sorted_files(Store::open(&project(dir.path())).unwrap().2), files);
	}

	#[test]
	fn an_upgrade_whose_rename_fails_removes_its_own_copy_and_keeps_the_old_file() {
		let dir = tempfile::tempdir().unwrap();
		let files = format_1_project(dir.path());

		let (opened, seen) = open_with_an_intruder(&project(dir.path()), |_| true);

		assert!(
			matches!(opened, Err(StoreError::UpgradeFailed(_))),
			"{:?}",
			opened.err()
		);
		assert_eq!(seen.len(), 2);
		assert!(
			seen.iter()
				.all(|intruder| intruder.refused_as_open && intruder.copy_still_there)
		);
		assert!(!upgrade_path(&project(dir.path())).exists());
		assert_eq!(read_closed(&project(dir.path())).unwrap().format, 1);
		assert_eq!(sorted_files(Store::open(&project(dir.path())).unwrap().2), files);
	}

	#[test]
	fn a_copy_that_does_not_match_the_old_file_is_refused() {
		let dir = tempfile::tempdir().unwrap();
		format_1_project(dir.path());
		let old = ReadOnlyDatabase::open(project(dir.path())).unwrap();
		let copy = dir.path().join("copy.cryarchive");
		let new = Database::builder()
			.create_file(create_new_file(&copy).unwrap())
			.unwrap();
		write_migrated(&old, &new).unwrap();
		drop(new);
		verify_migrated(&old, &copy).unwrap();

		let new = Database::open(&copy).unwrap();
		let txn = new.begin_write().unwrap();
		{
			let mut files = txn.open_table(FILES).unwrap();
			let key = file_key(ROOT_FOLDER, b"a.jpg");
			let changed = rmp_serde::to_vec_named(&FileRecord::discovered(1, 1)).unwrap();
			files
				.insert(key.as_slice(), Codec::built_in().pack(&changed).as_slice())
				.unwrap();
		}
		txn.commit().unwrap();
		drop(new);

		let checked = verify_migrated(&old, &copy);

		assert!(matches!(checked, Err(StoreError::UpgradeFailed(_))), "{checked:?}");
	}

	#[test]
	fn the_upgrade_copy_beside_a_newer_project_is_left_alone() {
		let dir = tempfile::tempdir().unwrap();
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store
			.put(Table::Meta, FORMAT_KEY.as_bytes().to_vec(), &(FORMAT_VERSION + 1))
			.unwrap();
		store.flush().unwrap();
		drop(store);
		std::fs::write(upgrade_path(&project(dir.path())), b"a newer build's copy").unwrap();

		assert!(matches!(Store::open(&project(dir.path())), Err(StoreError::TooNew(_))));
		assert_eq!(
			std::fs::read(upgrade_path(&project(dir.path()))).unwrap(),
			b"a newer build's copy"
		);
	}

	#[test]
	fn packed_values_that_cannot_be_read_back_are_refused() {
		let codec = Codec::built_in();
		let bytes = rmp_serde::to_vec_named(&committing()).unwrap();
		let packed = codec.pack(&bytes);
		assert_eq!(packed[0], LZ4);
		assert_eq!(codec.unpack(&packed).unwrap(), bytes);

		let claimed = u32::try_from(bytes.len() + 10).unwrap().to_le_bytes();
		let mut longer = packed.clone();
		longer[1..5].copy_from_slice(&claimed);
		let mut oversized = vec![LZ4];
		oversized.extend(u32::try_from(MAX_RECORD_BYTES + 1).unwrap().to_le_bytes());
		let unreadable = [
			vec![9, 1, 2],
			vec![LZ4, 1, 0],
			oversized,
			vec![LZ4, 10, 0, 0, 0, 0xFF, 0xFF],
			longer,
		];

		for stored in unreadable {
			assert!(
				matches!(codec.unpack(&stored), Err(StoreError::Damaged(_))),
				"{stored:?}"
			);
		}
	}

	#[test]
	fn values_over_the_record_limit_are_stored_raw() {
		let codec = Codec::built_in();
		let big = vec![0; MAX_RECORD_BYTES + 1];

		let stored = codec.pack(&big);

		assert_eq!(stored[0], RAW);
		assert_eq!(codec.unpack(&stored).unwrap(), big);
	}

	#[test]
	fn a_project_opened_after_a_crash_says_so() {
		use redb::StorageBackend;

		let dir = tempfile::tempdir().unwrap();
		let memory = SharedMemory(std::sync::Arc::default());
		let db = redb::Builder::new().create_with_backend(memory.clone()).unwrap();
		write_new(&db, &Settings::default(), &Codec::built_in()).unwrap();
		let mut bytes = vec![0; usize::try_from(memory.len().unwrap()).unwrap()];
		memory.read(0, &mut bytes).unwrap();
		std::mem::forget(db);
		std::fs::write(project(dir.path()), &bytes).unwrap();

		let (store, _, loaded) = Store::open(&project(dir.path())).unwrap();
		assert!(loaded.recovered);
		drop(store);

		assert!(!Store::open(&project(dir.path())).unwrap().2.recovered);
	}

	#[test]
	fn learned_values_outside_their_encoders_range_are_ignored_one_by_one() {
		let dir = tempfile::tempdir().unwrap();
		let video = "video/h264/1080/fps31/sdr8/progressive/clean/opaque|svt-av1-4.2.0|p5|t93.0/89.0";
		let good = GroupStats {
			setting: 29.5,
			slope: Some(0.4),
			bpp: Some(0.03),
			offset: None,
			observations: 3,
			without_grain: false,
		};
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store.set_quality(video, &good).unwrap();
		store
			.set_quality(&video.replace("h264", "hevc"), &GroupStats { setting: 70.0, ..good })
			.unwrap();
		store
			.set_quality(
				&video.replace("h264", "vp9"),
				&GroupStats {
					setting: f64::NAN,
					..good
				},
			)
			.unwrap();
		store.flush().unwrap();
		drop(store);

		let (_, _, loaded) = Store::open(&project(dir.path())).unwrap();

		assert_eq!(loaded.quality, vec![(video.to_owned(), good)]);
		assert_eq!(loaded.ignored_quality.len(), 2, "{:?}", loaded.ignored_quality);
		assert!(
			loaded
				.ignored_quality
				.iter()
				.any(|line| line.contains("video/hevc/") && line.contains("70")),
			"{:?}",
			loaded.ignored_quality
		);
		assert!(
			loaded
				.ignored_quality
				.iter()
				.any(|line| line.contains("video/vp9/") && line.contains("NaN")),
			"{:?}",
			loaded.ignored_quality
		);
	}
}
