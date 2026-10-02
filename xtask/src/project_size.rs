use std::fs;
use std::path::Path;

use cryarchiver_project::keys::RelPath;
use cryarchiver_project::record::{
	FailureLog, FileRecord, FileState, FolderRecord, Hash, How, Identity, IssueKind, MediaClass, Output,
};
use cryarchiver_project::settings::Settings;
use cryarchiver_project::store::{BATCH_CHANGES, Store};

const FAILING_EVERY: usize = 50;
const FILES_PER_FOLDER: usize = 100;
const TAIL_LINES: usize = 120;

pub fn run(files: usize) -> Result<(), String> {
	let dir = tempfile::tempdir().map_err(text)?;
	let path = dir.path().join("size.cryarchive");
	let mut store = Store::create(&path, &Settings::default()).map_err(text)?;

	for index in 0..files {
		let key = file_path(index);
		store.set_file(&key, &record(index, &key)).map_err(text)?;
		if index.is_multiple_of(FAILING_EVERY) {
			store.set_log(&key, 1, &failure(index)).map_err(text)?;
		}

		if index.is_multiple_of(FILES_PER_FOLDER) {
			let folder = FolderRecord {
				scanned: true,
				timestamps_applied: true,
				..FolderRecord::default()
			};
			store
				.set_folder(&key.parent().unwrap_or_default(), &folder)
				.map_err(text)?;
		}

		if store.pending_changes() >= BATCH_CHANGES {
			store.flush().map_err(text)?;
		}
	}
	store.flush().map_err(text)?;

	print_sizes(&store, &path, "written")?;
	store.compact().map_err(text)?;
	print_sizes(&store, &path, "compacted")
}

fn text(err: impl std::fmt::Display) -> String {
	err.to_string()
}

fn file_path(index: usize) -> RelPath {
	RelPath::from_bytes(
		format!(
			"Library/{}/Event {:03}/IMG_{index:06}.jpg",
			2000 + index / 20_000,
			index / FILES_PER_FOLDER % 1000
		)
		.into_bytes(),
	)
}

fn record(index: usize, key: &RelPath) -> FileRecord {
	let seed = u64::try_from(index).unwrap_or(u64::MAX);
	let stem = key.as_bytes().strip_suffix(b".jpg").unwrap_or(key.as_bytes());
	let output = Output {
		how: How::Encoded,
		path: RelPath::from_bytes([stem, b".cry.avif"].concat()),
		size: 1_200_000 + seed % 400_000,
		hash: Some(hash(seed ^ 0x55)),
	};
	let state = if index.is_multiple_of(FAILING_EVERY) {
		FileState::Issue {
			kind: IssueKind::Unrecoverable { damaged: true },
			attempt: 1,
			reason: "source damaged: the image is cut short".into(),
			output: None,
		}
	} else {
		FileState::Done(output)
	};

	FileRecord {
		size: 3_000_000 + seed % 2_000_000,
		mtime_ns: 1_600_000_000_000_000_000_i64
			.wrapping_add(i64::try_from(seed).unwrap_or(0).wrapping_mul(1_000_000_007)),
		identity: Some(Identity { volume: 7, index: seed }),
		class: MediaClass::Media,
		window_hash: Some(hash(seed)),
		full_hash: Some(hash(seed ^ 0xAA)),
		replaces: None,
		state,
	}
}

fn hash(seed: u64) -> Hash {
	let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
	let mut bytes = [0; 32];
	for byte in &mut bytes {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		*byte = state.to_le_bytes()[0];
	}
	Hash(bytes)
}

fn failure(index: usize) -> FailureLog {
	FailureLog {
		error: "source damaged: the image is cut short".into(),
		log_tail: (0..TAIL_LINES)
			.map(|line| {
				format!(
					"[mjpeg @ 0x{index:012x}] error count: {line} (overread at byte {})",
					line * 4096 + index
				)
			})
			.collect(),
	}
}

fn print_sizes(store: &Store, path: &Path, label: &str) -> Result<(), String> {
	println!("{label}: {} bytes", fs::metadata(path).map_err(text)?.len());
	for (table, size) in store.table_sizes().map_err(text)? {
		println!(
			"  {table:<10} {:>9} entries {:>12} stored {:>10} metadata {:>10} fragmented",
			size.entries, size.stored_bytes, size.metadata_bytes, size.fragmented_bytes
		);
	}
	Ok(())
}
