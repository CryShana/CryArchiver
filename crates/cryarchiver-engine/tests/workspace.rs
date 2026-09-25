use std::fs;

use cryarchiver_engine::workspace::{CLI_WORK_DIR, Workspace, sweep};

#[test]
fn creates_locked_job_folder_and_removes_it_on_drop() {
	let out = tempfile::tempdir().unwrap();
	let root = out.path().join(CLI_WORK_DIR);

	let workspace = Workspace::create(&root, "music/a.flac".as_ref()).unwrap();
	let dir = workspace.dir().to_path_buf();

	assert!(dir.join("job.lock").is_file());
	assert!(fs::read_to_string(dir.join("job.json")).unwrap().contains("a.flac"));
	assert_eq!(workspace.file("audio.opus"), dir.join("audio.opus"));

	fs::write(workspace.file("audio.opus"), b"partial").unwrap();
	drop(workspace);

	assert!(!dir.exists());
	assert!(!root.exists());
}

#[test]
fn sweep_removes_abandoned_and_keeps_live_jobs() {
	let out = tempfile::tempdir().unwrap();
	let root = out.path().join(CLI_WORK_DIR);
	let abandoned = root.join("01J9Z3K8QXABANDONED0000000");
	fs::create_dir_all(&abandoned).unwrap();
	fs::write(abandoned.join("job.lock"), b"").unwrap();
	fs::write(abandoned.join("video.a1.mkv"), b"partial").unwrap();

	let live = Workspace::create(&root, "b.flac".as_ref()).unwrap();

	assert_eq!(sweep(&root), 1);
	assert!(!abandoned.exists());
	assert!(live.dir().exists());
}

#[test]
fn root_survives_while_another_job_runs() {
	let out = tempfile::tempdir().unwrap();
	let root = out.path().join(CLI_WORK_DIR);

	let first = Workspace::create(&root, "a.flac".as_ref()).unwrap();
	let second = Workspace::create(&root, "b.flac".as_ref()).unwrap();
	drop(first);

	assert!(root.exists());
	assert!(second.dir().exists());
}

#[test]
fn sweep_keeps_folders_whose_lock_cannot_be_opened() {
	let out = tempfile::tempdir().unwrap();
	let root = out.path().join(CLI_WORK_DIR);

	let a = root.join("A");
	fs::create_dir_all(a.join("job.lock")).unwrap();

	let b = root.join("B");
	fs::create_dir_all(&b).unwrap();

	assert_eq!(sweep(&root), 1);
	assert!(a.exists());
	assert!(!b.exists());
}
