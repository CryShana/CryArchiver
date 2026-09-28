//! The project file: one redb database (project layer spec §3). Write-ahead intents are durable before their action
//! runs; every other change waits in a batch that commits every 1000 changes or 250 ms, whichever comes first.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cryarchiver_engine::quality::memory::GroupStats;
use redb::{Database, ReadOnlyDatabase, ReadableDatabase, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::keys::RelPath;
use crate::record::{FailureLog, FileRecord, FolderRecord, UnknownRecord};
use crate::settings::Settings;

pub const FORMAT_VERSION: u32 = 1;
pub const BATCH_CHANGES: usize = 1000;
pub const BATCH_INTERVAL: Duration = Duration::from_millis(250);

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const FILES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("files");
const FOLDERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("folders");
const LOGS: TableDefinition<(&[u8], u32), &[u8]> = TableDefinition::new("logs");
const UNKNOWN: TableDefinition<&[u8], &[u8]> = TableDefinition::new("unknown");

const FORMAT_KEY: &str = "format";
const APP_VERSION_KEY: &str = "app_version";
const CREATED_KEY: &str = "created";
const SETTINGS_KEY: &str = "settings";
const QUALITY_PREFIX: &str = "quality/";

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
	redb::CommitError
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
	Meta,
	Files,
	Folders,
	Logs(u32),
	Unknown,
}

#[derive(Debug)]
struct Change {
	table: Table,
	key: Vec<u8>,
	value: Option<Vec<u8>>,
}

pub struct Store {
	db: Database,
	pending: Vec<Change>,
	oldest_pending: Option<Instant>,
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
		write_new(&db, settings)?;

		Ok(Self {
			db,
			pending: Vec::new(),
			oldest_pending: None,
		})
	}

	pub fn open(path: &Path) -> Result<(Self, Meta, Loaded), StoreError> {
		require_a_file(path)?;
		refuse_other_names(&open_in_place(path)?)?;

		let db = Database::open(path)?;
		let meta = read_meta(&db)?;
		if meta.format > FORMAT_VERSION {
			return Err(StoreError::TooNew(meta.format));
		}

		let mut store = Self {
			db,
			pending: Vec::new(),
			oldest_pending: None,
		};
		store.upgrade(meta.format)?;
		let loaded = store.load()?;
		Ok((store, meta, loaded))
	}

	// Each schema change adds a step from its predecessor; format 1 is the first.
	fn upgrade(&mut self, from: u32) -> Result<(), StoreError> {
		if from == FORMAT_VERSION {
			return Ok(());
		}

		self.put(Table::Meta, FORMAT_KEY.as_bytes().to_vec(), &FORMAT_VERSION)?;
		self.flush()
	}

	fn load(&self) -> Result<Loaded, StoreError> {
		let txn = self.db.begin_read()?;
		let mut loaded = Loaded::default();

		for entry in txn.open_table(FILES)?.iter()? {
			let (key, value) = entry?;
			loaded
				.files
				.push((RelPath::from_bytes(key.value().to_vec()), decode(value.value())?));
		}

		for entry in txn.open_table(FOLDERS)?.iter()? {
			let (key, value) = entry?;
			loaded
				.folders
				.push((RelPath::from_bytes(key.value().to_vec()), decode(value.value())?));
		}

		for entry in txn.open_table(UNKNOWN)?.iter()? {
			let (key, value) = entry?;
			loaded
				.unknown
				.push((RelPath::from_bytes(key.value().to_vec()), decode(value.value())?));
		}

		for entry in txn.open_table(LOGS)?.iter()? {
			let (key, _) = entry?;
			let (path, attempt) = key.value();
			let last = loaded
				.last_attempts
				.entry(RelPath::from_bytes(path.to_vec()))
				.or_default();
			*last = (*last).max(attempt);
		}

		// Learned quality is only a cache: an entry this build can't read, damaged or from a later build, is skipped
		// rather than keeping the project from opening.
		for entry in txn.open_table(META)?.iter()? {
			let (key, value) = entry?;
			if let Some(quality_key) = key.value().strip_prefix(QUALITY_PREFIX)
				&& let Ok(stats) = decode(value.value())
			{
				loaded.quality.push((quality_key.to_owned(), stats));
			}
		}
		Ok(loaded)
	}

	pub fn set_file(&mut self, path: &RelPath, record: &FileRecord) -> Result<(), StoreError> {
		self.put(Table::Files, path.as_bytes().to_vec(), record)
	}

	pub fn remove_file(&mut self, path: &RelPath) {
		self.remove(Table::Files, path.as_bytes().to_vec());
	}

	pub fn set_folder(&mut self, path: &RelPath, record: &FolderRecord) -> Result<(), StoreError> {
		self.put(Table::Folders, path.as_bytes().to_vec(), record)
	}

	pub fn set_log(&mut self, path: &RelPath, attempt: u32, log: &FailureLog) -> Result<(), StoreError> {
		self.put(Table::Logs(attempt), path.as_bytes().to_vec(), log)
	}

	pub fn set_unknown(&mut self, path: &RelPath, record: &UnknownRecord) -> Result<(), StoreError> {
		self.put(Table::Unknown, path.as_bytes().to_vec(), record)
	}

	pub fn remove_unknown(&mut self, path: &RelPath) {
		self.remove(Table::Unknown, path.as_bytes().to_vec());
	}

	pub fn set_settings(&mut self, settings: &Settings) -> Result<(), StoreError> {
		self.put(Table::Meta, SETTINGS_KEY.as_bytes().to_vec(), settings)
	}

	pub fn set_quality(&mut self, key: &str, stats: &GroupStats) -> Result<(), StoreError> {
		self.put(Table::Meta, format!("{QUALITY_PREFIX}{key}").into_bytes(), stats)
	}

	/// A write-ahead intent: commits it together with everything batched so far, durably, before returning.
	pub fn write_intent(&mut self, path: &RelPath, record: &FileRecord) -> Result<(), StoreError> {
		self.set_file(path, record)?;
		self.flush()
	}

	pub fn pending_changes(&self) -> usize {
		self.pending.len()
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
				}
			}
		}
		txn.commit()?;

		self.pending.clear();
		self.oldest_pending = None;
		Ok(())
	}

	pub fn read_log(&self, path: &RelPath, attempt: u32) -> Result<Option<FailureLog>, StoreError> {
		let batched = self
			.pending
			.iter()
			.rev()
			.find(|change| change.table == Table::Logs(attempt) && change.key == path.as_bytes());
		if let Some(change) = batched {
			return change.value.as_deref().map(decode).transpose();
		}

		let txn = self.db.begin_read()?;
		let logs = txn.open_table(LOGS)?;
		logs.get((path.as_bytes(), attempt))?
			.map(|value| decode(value.value()))
			.transpose()
	}

	fn put<T: Serialize + ?Sized>(&mut self, table: Table, key: Vec<u8>, value: &T) -> Result<(), StoreError> {
		let value = rmp_serde::to_vec_named(value)?;
		self.push(Change {
			table,
			key,
			value: Some(value),
		});
		Ok(())
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

fn write_new(db: &Database, settings: &Settings) -> Result<(), StoreError> {
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
		txn.open_table(FILES)?;
		txn.open_table(FOLDERS)?;
		txn.open_table(LOGS)?;
		txn.open_table(UNKNOWN)?;
	}
	txn.commit()?;
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

/// Copies the file into memory under a shared lock and lets redb recover the copy, so the settings of a project that
/// wasn't closed cleanly can be read without opening it.
fn read_unclean(path: &Path, limit: u64) -> Result<Meta, StoreError> {
	use std::fs::TryLockError;
	use std::io::Read;

	let mut file = std::fs::File::open(path)?;
	if file.metadata()?.len() > limit {
		return Err(StoreError::NeedsRecovery);
	}

	match file.try_lock_shared() {
		Ok(()) => {}
		Err(TryLockError::WouldBlock) => return Err(StoreError::AlreadyOpen),
		Err(TryLockError::Error(err)) => return Err(err.into()),
	}
	let mut bytes = Vec::new();
	file.read_to_end(&mut bytes)?;
	drop(file);

	let db = redb::Builder::new()
		.create_with_backend(memory_backend(&bytes)?)
		.map_err(|_| StoreError::NeedsRecovery)?;
	read_meta(&db)
}

fn memory_backend(bytes: &[u8]) -> io::Result<redb::backends::InMemoryBackend> {
	use redb::StorageBackend;

	let backend = redb::backends::InMemoryBackend::new();
	backend.set_len(bytes.len() as u64)?;
	backend.write(0, bytes)?;
	Ok(backend)
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

	let txn = db.begin_read()?;
	writeln!(out, "\n[files]")?;
	for entry in txn.open_table(FILES)?.iter()? {
		let (key, value) = entry?;
		let record: FileRecord = decode(value.value())?;
		writeln!(out, "{:?} = {record:?}", RelPath::from_bytes(key.value().to_vec()))?;
	}

	writeln!(out, "\n[folders]")?;
	for entry in txn.open_table(FOLDERS)?.iter()? {
		let (key, value) = entry?;
		let record: FolderRecord = decode(value.value())?;
		writeln!(out, "{:?} = {record:?}", RelPath::from_bytes(key.value().to_vec()))?;
	}

	writeln!(out, "\n[logs]")?;
	for entry in txn.open_table(LOGS)?.iter()? {
		let (key, value) = entry?;
		let (path, attempt) = key.value();
		let log: FailureLog = decode(value.value())?;
		writeln!(
			out,
			"{:?} attempt {attempt}: {}",
			RelPath::from_bytes(path.to_vec()),
			log.error
		)?;
		for line in &log.log_tail {
			writeln!(out, "    {line}")?;
		}
	}

	writeln!(out, "\n[unknown]")?;
	for entry in txn.open_table(UNKNOWN)?.iter()? {
		let (key, value) = entry?;
		let record: UnknownRecord = decode(value.value())?;
		writeln!(out, "{:?} = {record:?}", RelPath::from_bytes(key.value().to_vec()))?;
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
mod tests {
	use std::ffi::OsStr;

	use super::*;
	use crate::record::{CopyReason, FileState, Hash, How, Output};

	fn path(name: &str) -> RelPath {
		RelPath::root().join(OsStr::new(name))
	}

	fn project(dir: &Path) -> std::path::PathBuf {
		dir.join("archive.cryarchive")
	}

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
		write_new(&db, &settings).unwrap();
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

		assert!(text.contains("format = 1"), "{text}");
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
			observations: 2,
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
	fn an_unreadable_quality_entry_is_skipped_and_the_project_still_opens() {
		let dir = tempfile::tempdir().unwrap();
		let stats = GroupStats {
			setting: 88.0,
			slope: None,
			bpp: None,
			observations: 1,
		};
		let mut store = Store::create(&project(dir.path()), &Settings::default()).unwrap();
		store.set_quality("good", &stats).unwrap();
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

		assert_eq!(loaded.quality, vec![("good".to_owned(), stats)]);
		assert!(text.contains("good = GroupStats"), "{text}");
		assert!(text.contains("x = unreadable"), "{text}");
	}
}
