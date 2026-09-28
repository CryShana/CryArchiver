use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cryarchiver_engine::sink::ArchivalSink;
use cryarchiver_engine::workspace::{
	ARCHIVE_DIR, ARCHIVE_WORK_DIR, CLI_WORK_DIR, CheckedFolders, RealFolders, UnsafeFolder, Workspace,
	check_real_folders, create_real_folders, sweep,
};

/// A second name for the folder `target`: a junction on Windows, a symbolic link elsewhere. False when one can't be
/// made here.
fn link_folder(link: &Path, target: &Path) -> bool {
	#[cfg(windows)]
	return std::process::Command::new("cmd")
		.args(["/C", "mklink", "/J"])
		.arg(link)
		.arg(target)
		.output()
		.is_ok_and(|out| out.status.success());

	#[cfg(unix)]
	return std::os::unix::fs::symlink(target, link).is_ok();
}

fn link_file(link: &Path, target: &Path) -> bool {
	#[cfg(windows)]
	return std::os::windows::fs::symlink_file(target, link).is_ok();

	#[cfg(unix)]
	return std::os::unix::fs::symlink(target, link).is_ok();
}

/// A folder outside every work area, holding a folder with no `job.lock` and a file.
fn outside(dir: &Path) -> std::path::PathBuf {
	let outside = dir.join("outside");
	fs::create_dir_all(outside.join("victim")).unwrap();
	fs::write(outside.join("victim").join("keep.txt"), b"keep").unwrap();
	fs::write(outside.join("top.txt"), b"top").unwrap();
	outside
}

fn assert_untouched(outside: &Path) {
	assert_eq!(fs::read(outside.join("victim").join("keep.txt")).unwrap(), b"keep");
	assert_eq!(fs::read(outside.join("top.txt")).unwrap(), b"top");
	assert_eq!(fs::read_dir(outside).unwrap().count(), 2);
}

#[test]
fn sweep_never_lists_through_a_root_that_is_a_link() {
	let out = tempfile::tempdir().unwrap();
	let outside = outside(out.path());
	let root = out.path().join(CLI_WORK_DIR);
	if !link_folder(&root, &outside) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}

	assert_eq!(sweep(&root), 0);

	assert_untouched(&outside);
	assert!(fs::symlink_metadata(&root).is_ok());
}

#[test]
fn sweep_removes_links_in_the_work_area_but_never_what_they_lead_to() {
	let out = tempfile::tempdir().unwrap();
	let outside = outside(out.path());
	let root = out.path().join(CLI_WORK_DIR);
	fs::create_dir(&root).unwrap();
	let linked = root.join("01J9Z3K8QXLINKEDFOLDER0000");
	if !link_folder(&linked, &outside.join("victim")) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}
	let file_link = root.join("file-link");
	let file_linked = link_file(&file_link, &outside.join("top.txt"));

	sweep(&root);

	assert!(fs::symlink_metadata(&linked).is_err());
	if file_linked {
		assert!(fs::symlink_metadata(&file_link).is_err());
	} else {
		eprintln!("skipped the file link: can't create one here");
	}
	assert_untouched(&outside);
}

#[test]
fn folders_are_made_one_name_at_a_time_and_never_through_a_link() {
	let out = tempfile::tempdir().unwrap();
	let outside = outside(out.path());
	let base = out.path().join("archive");
	fs::create_dir(&base).unwrap();

	let mut checked = Vec::new();
	let made = create_real_folders(&base, &base.join("a").join("b"), |folder| {
		checked.push(folder.to_path_buf());
		Ok(())
	})
	.unwrap();

	assert_eq!(made, [base.join("a"), base.join("a").join("b")]);
	assert_eq!(checked, [base.clone(), base.join("a")]);
	assert!(check_real_folders(&base, &base.join("a").join("b")).is_ok());
	assert!(check_real_folders(&base, &base.join("missing")).is_err());
	assert!(!base.join("missing").exists());

	let refused = create_real_folders(&base, &base.join("a").join("c"), |_| Err(io::Error::other("refused")));
	assert!(refused.is_err());
	assert!(!base.join("a").join("c").exists());

	fs::write(base.join("file.txt"), b"x").unwrap();
	let in_the_way = create_real_folders(&base, &base.join("file.txt").join("x"), |_| Ok(())).unwrap_err();
	assert_eq!(in_the_way.kind(), io::ErrorKind::NotADirectory);
	assert!(create_real_folders(&base, &base.join("..").join("x"), |_| Ok(())).is_err());
	assert!(!out.path().join("x").exists());

	let link = base.join("link");
	if !link_folder(&link, &outside) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}
	for folder in [link.clone(), link.join("new"), link.join("victim")] {
		let refused = create_real_folders(&base, &folder, |_| Ok(())).unwrap_err();
		assert_eq!(
			UnsafeFolder::of(&refused).map(|unsafe_folder| &unsafe_folder.path),
			Some(&link)
		);
		assert!(check_real_folders(&base, &folder).is_err());
	}
	assert!(create_real_folders(&link, &link.join("new"), |_| Ok(())).is_err());
	assert_untouched(&outside);
}

#[test]
fn a_workspace_is_never_made_through_a_link() {
	let out = tempfile::tempdir().unwrap();
	let outside = outside(out.path());
	let root = out.path().join(CLI_WORK_DIR);
	if !link_folder(&root, &outside) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}

	assert!(Workspace::create(&root, "a.flac".as_ref()).is_err());

	assert_untouched(&outside);
}

/// A destination beside `outside`, and a user's empty `work` folder in `outside`.
fn archive_beside(out: &Path, outside: &Path) -> PathBuf {
	let archive = out.join("archive");
	fs::create_dir(&archive).unwrap();
	fs::create_dir(outside.join("work")).unwrap();
	archive
}

#[test]
fn an_archive_workspace_is_never_made_through_a_link_above_its_root() {
	let out = tempfile::tempdir().unwrap();
	let outside = outside(out.path());
	let archive = archive_beside(out.path(), &outside);
	let link = archive.join(ARCHIVE_DIR);
	if !link_folder(&link, &outside) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}

	let refused = Workspace::for_sink(&ArchivalSink::new(&archive), "a.flac".as_ref())
		.err()
		.expect("the workspace was made through the link");

	assert_eq!(UnsafeFolder::of(&refused).map(|folder| &folder.path), Some(&link));
	assert_eq!(fs::read_dir(outside.join("work")).unwrap().count(), 0);
	assert_eq!(fs::read_dir(&outside).unwrap().count(), 3);
	assert_untouched_but_work(&outside);
}

#[test]
fn an_archive_workspace_removes_nothing_through_a_link_planted_while_it_runs() {
	let out = tempfile::tempdir().unwrap();
	let outside = outside(out.path());
	let archive = archive_beside(out.path(), &outside);
	let workspace = Workspace::for_sink(&ArchivalSink::new(&archive), "a.flac".as_ref()).unwrap();
	let job = workspace.dir().file_name().unwrap().to_owned();
	assert_eq!(
		workspace.dir(),
		archive.join(ARCHIVE_DIR).join(ARCHIVE_WORK_DIR).join(&job)
	);

	if fs::rename(archive.join(ARCHIVE_DIR), archive.join("moved")).is_err() {
		eprintln!("skipped: the work area can't be moved while its job holds the lock here");
		return;
	}
	fs::create_dir(outside.join("work").join(&job)).unwrap();
	fs::write(outside.join("work").join(&job).join("keep.txt"), b"keep").unwrap();
	if !link_folder(&archive.join(ARCHIVE_DIR), &outside) {
		eprintln!("skipped: no folder link can be made here");
		return;
	}
	drop(workspace);

	assert_eq!(
		fs::read(outside.join("work").join(&job).join("keep.txt")).unwrap(),
		b"keep"
	);
	assert_untouched_but_work(&outside);
}

/// Real folders below the base, until `refuse` is set.
#[derive(Debug)]
struct Refusable {
	real: RealFolders,
	refuse: AtomicBool,
}

impl CheckedFolders for Refusable {
	fn prepare(&self, folder: &Path) -> io::Result<()> {
		self.real.prepare(folder)
	}

	fn check(&self, folder: &Path) -> io::Result<()> {
		if self.refuse.load(Ordering::SeqCst) {
			return Err(io::Error::other("refused"));
		}
		self.real.check(folder)
	}
}

#[test]
fn an_archive_workspace_is_removed_only_while_its_folders_still_pass_their_check() {
	let out = tempfile::tempdir().unwrap();
	let archive = out.path().join("archive");
	fs::create_dir(&archive).unwrap();
	let folders = Arc::new(Refusable {
		real: RealFolders(archive.clone()),
		refuse: AtomicBool::new(false),
	});
	let root = archive.join(ARCHIVE_DIR).join(ARCHIVE_WORK_DIR);

	let workspace = Workspace::create_checked(&root, "a.flac".as_ref(), folders.clone()).unwrap();
	let dir = workspace.dir().to_path_buf();
	folders.refuse.store(true, Ordering::SeqCst);
	drop(workspace);

	assert!(dir.join("job.lock").is_file());
	assert!(dir.join("job.json").is_file());
}

#[test]
fn an_archive_workspace_is_made_one_folder_at_a_time_and_removed_after() {
	let out = tempfile::tempdir().unwrap();
	let archive = out.path().join("archive");
	fs::create_dir(&archive).unwrap();

	let workspace = Workspace::for_sink(&ArchivalSink::new(&archive), "a.flac".as_ref()).unwrap();
	let dir = workspace.dir().to_path_buf();
	assert!(dir.join("job.lock").is_file());
	drop(workspace);

	assert!(!dir.exists());
	assert!(!archive.join(ARCHIVE_DIR).join(ARCHIVE_WORK_DIR).exists());
	assert!(Workspace::for_sink(&ArchivalSink::new(&out.path().join("missing")), "a.flac".as_ref()).is_err());
	assert!(!out.path().join("missing").exists());
}

fn assert_untouched_but_work(outside: &Path) {
	assert_eq!(fs::read(outside.join("victim").join("keep.txt")).unwrap(), b"keep");
	assert_eq!(fs::read(outside.join("top.txt")).unwrap(), b"top");
	assert!(outside.join("work").is_dir());
}

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
