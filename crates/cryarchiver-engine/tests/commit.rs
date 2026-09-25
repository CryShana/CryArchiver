use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, SystemTime};

use cryarchiver_engine::commit::{
	Artifact, CommitError, Replace, SourceTimes, check_space, commit_file, fallback_no_replace, free_space,
	rename_no_replace, same_file,
};
use cryarchiver_engine::outcome::ErrorKind as JobErrorKind;

fn staged(dir: &Path, content: &[u8], source: &Path) -> Artifact {
	let path = dir.join("mux.opus");
	fs::write(&path, content).unwrap();
	Artifact {
		path,
		source: source.to_path_buf(),
		times: None,
	}
}

#[test]
fn rename_moves_when_the_target_is_free() {
	let dir = tempfile::tempdir().unwrap();
	let (from, to) = (dir.path().join("mux.opus"), dir.path().join("a.cry.opus"));
	fs::write(&from, b"new").unwrap();

	rename_no_replace(&from, &to).unwrap();

	assert!(!from.exists());
	assert_eq!(fs::read(&to).unwrap(), b"new");
}

#[test]
fn rename_never_replaces() {
	let dir = tempfile::tempdir().unwrap();
	let (from, to) = (dir.path().join("mux.opus"), dir.path().join("a.cry.opus"));
	fs::write(&from, b"new").unwrap();
	fs::write(&to, b"precious").unwrap();

	let err = rename_no_replace(&from, &to).unwrap_err();

	assert_eq!(err.kind(), ErrorKind::AlreadyExists);
	assert_eq!(fs::read(&to).unwrap(), b"precious");
	assert_eq!(fs::read(&from).unwrap(), b"new");
}

#[test]
fn fallback_commit_never_overwrites() {
	let dir = tempfile::tempdir().unwrap();
	let (from, to) = (dir.path().join("mux.opus"), dir.path().join("a.cry.opus"));
	fs::write(&from, b"new").unwrap();
	fs::write(&to, b"precious").unwrap();

	assert_eq!(
		fallback_no_replace(&from, &to).unwrap_err().kind(),
		ErrorKind::AlreadyExists
	);
	assert_eq!(fs::read(&to).unwrap(), b"precious");

	fs::remove_file(&to).unwrap();
	fallback_no_replace(&from, &to).unwrap();
	assert!(!from.exists());
	assert_eq!(fs::read(&to).unwrap(), b"new");
}

#[test]
fn commit_file_reports_existing_targets() {
	let dir = tempfile::tempdir().unwrap();
	let target = dir.path().join("a.cry.opus");
	fs::write(&target, b"precious").unwrap();

	let result = commit_file(
		&staged(dir.path(), b"new", Path::new("a.flac")),
		&target,
		Replace::Never,
	);

	assert!(matches!(result, Err(CommitError::Exists(path)) if path == target));
	assert_eq!(fs::read(&target).unwrap(), b"precious");
}

#[test]
fn commit_file_applies_the_source_times() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("a.flac");
	fs::write(&source, b"src").unwrap();
	let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
	File::options()
		.write(true)
		.open(&source)
		.unwrap()
		.set_modified(old)
		.unwrap();
	let target = dir.path().join("a.cry.opus");

	let mut artifact = staged(dir.path(), b"new", &source);
	artifact.times = Some(SourceTimes::from_metadata(&fs::metadata(&source).unwrap()).unwrap());
	commit_file(&artifact, &target, Replace::Never).unwrap();

	assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), old);
}

#[test]
fn replacing_an_output_never_replaces_the_source() {
	let dir = tempfile::tempdir().unwrap();
	let source = dir.path().join("a.flac");
	let target = dir.path().join("a.cry.opus");
	fs::write(&source, b"src").unwrap();
	fs::write(&target, b"old").unwrap();

	commit_file(&staged(dir.path(), b"new", &source), &target, Replace::ExistingOutput).unwrap();
	assert_eq!(fs::read(&target).unwrap(), b"new");

	let result = commit_file(&staged(dir.path(), b"newer", &source), &source, Replace::ExistingOutput);
	assert!(matches!(result, Err(CommitError::IsSource(_))));
	assert_eq!(fs::read(&source).unwrap(), b"src");
}

#[test]
fn space_checks_use_the_110_percent_rule() {
	let dir = tempfile::tempdir().unwrap();
	let free = free_space(dir.path()).unwrap();

	assert!(free > 0);
	assert_eq!(check_space(dir.path(), 1_000), Ok(()));
	assert!(
		matches!(check_space(dir.path(), free), Err(JobErrorKind::InsufficientSpace { need, .. }) if need == free + free / 10)
	);
}

#[test]
fn space_checks_work_for_relative_folders_that_do_not_exist_yet() {
	let missing = Path::new("no-such-dir-cryarchiver-test");

	let result = check_space(&missing.join(".cry-work"), u64::MAX / 4);

	assert!(
		matches!(result, Err(JobErrorKind::InsufficientSpace { .. })),
		"{result:?}"
	);
	assert!(!missing.exists());
}

#[test]
fn identifies_the_same_file_through_different_paths() {
	let dir = tempfile::tempdir().unwrap();
	let a = dir.path().join("a.flac");
	let b = dir.path().join("b.flac");
	fs::write(&a, b"a").unwrap();
	fs::write(&b, b"b").unwrap();

	assert!(same_file(&a, &dir.path().join(".").join("a.flac")).unwrap());
	assert!(!same_file(&a, &b).unwrap());
}
