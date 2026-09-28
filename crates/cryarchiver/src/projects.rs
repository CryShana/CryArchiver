//! Opening, creating and dumping archive projects from the command line.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cryarchiver_engine::preset::Speed;
use cryarchiver_engine::process::limits::{self, Limits};
use cryarchiver_engine::text::display_path;
use cryarchiver_engine::toolchain::{ToolConfig, Toolchain};
use cryarchiver_project::backend::EngineBackend;
use cryarchiver_project::settings::Speed as ProjectSpeed;
use cryarchiver_project::store::StoreError;
use cryarchiver_project::{Project, ProjectError, ProjectOptions, Tuning};
use tokio_util::sync::CancellationToken;

use crate::block_on;

/// Checks ffmpeg, then opens the project with the real engine.
pub fn open(path: &Path, ffmpeg: Option<PathBuf>) -> Result<Project, String> {
	// Probing ffmpeg starts tools, and on Windows a tool started before the first `apply` stays outside the job object
	// that carries the project's limit.
	limits::apply(Limits::default());

	let tools = block_on(Toolchain::detect(
		&ToolConfig {
			ffmpeg,
			cache_dir: None,
			refresh: false,
		},
		&CancellationToken::new(),
	))
	.map_err(|err| err.to_string())?;

	let universal = tools.universal_problems();
	if !universal.is_empty() {
		return Err(format!(
			"{}; run \"cryarchiver doctor\" after changing ffmpeg",
			universal.join("; ")
		));
	}

	let backend: Arc<EngineBackend> = Arc::new(EngineBackend::new(tools));
	retry_once_if_busy(BUSY_RETRY, || {
		Project::open(
			path,
			ProjectOptions {
				backend: backend.clone(),
				tuning: Tuning::default(),
				fault: None,
			},
		)
	})
	.map_err(|err| format!("{}: {err}", display_path(path)))
}

const BUSY_RETRY: Duration = Duration::from_millis(250);

/// A start menu, this one or another instance's, reads each recent project for a moment; an open that lands in that
/// moment is refused as already open, so it is tried once more.
fn retry_once_if_busy<T>(
	delay: Duration,
	mut open: impl FnMut() -> Result<T, ProjectError>,
) -> Result<T, ProjectError> {
	match open() {
		Err(ProjectError::Store(StoreError::AlreadyOpen)) => {
			std::thread::sleep(delay);
			open()
		}
		opened => opened,
	}
}

pub fn create(
	source: &Path,
	destination: &Path,
	name: Option<&str>,
	speed: Speed,
	watch: bool,
	limit: Option<u8>,
) -> u8 {
	let name = name.map_or_else(
		|| source.file_name().unwrap_or(OsStr::new("archive")).to_os_string(),
		Into::into,
	);
	let created = cryarchiver_project::create(source, destination, &name, |settings| {
		settings.speed = match speed {
			Speed::Slow => ProjectSpeed::Slow,
			Speed::Moderate => ProjectSpeed::Moderate,
			Speed::Fast => ProjectSpeed::Fast,
		};
		settings.watch = watch;
		settings.performance_limit = limit;
	});

	match created {
		Ok(path) => {
			println!("created {}", display_path(&path));
			println!(
				"open it with \"cryarchiver {}\" or run it with \"cryarchiver run {}\"",
				display_path(&path),
				display_path(&path)
			);
			0
		}
		Err(err) => {
			eprintln!("error: {err}");
			2
		}
	}
}

pub fn dump(path: &Path) -> u8 {
	match cryarchiver_project::store::dump(path, &mut io::stdout()) {
		Ok(()) => 0,
		Err(err) => {
			eprintln!("error: {}: {err}", display_path(path));
			2
		}
	}
}

/// The first Ctrl+C asks the run to stop now; a second one exits at once. The tools end with the process, and the
/// next open requeues what was running.
pub fn stop_on_ctrl_c(interrupted: &Arc<AtomicBool>) {
	let interrupted = interrupted.clone();
	std::thread::spawn(move || {
		let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
			return;
		};

		runtime.block_on(async {
			if tokio::signal::ctrl_c().await.is_err() {
				return;
			}
			interrupted.store(true, Ordering::SeqCst);

			if tokio::signal::ctrl_c().await.is_ok() {
				std::process::exit(130);
			}
		});
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_open_refused_as_already_open_is_tried_once_more() {
		let mut calls = 0;
		let opened = retry_once_if_busy(Duration::ZERO, || {
			calls += 1;
			if calls == 1 {
				Err(ProjectError::Store(StoreError::AlreadyOpen))
			} else {
				Ok(calls)
			}
		});
		assert_eq!(opened.unwrap(), 2);

		let mut calls = 0;
		let refused: Result<(), _> = retry_once_if_busy(Duration::ZERO, || {
			calls += 1;
			Err(ProjectError::Store(StoreError::AlreadyOpen))
		});
		assert!(refused.is_err());
		assert_eq!(calls, 2);

		let mut calls = 0;
		let failed: Result<(), _> = retry_once_if_busy(Duration::ZERO, || {
			calls += 1;
			Err(ProjectError::Store(StoreError::Missing))
		});
		assert!(failed.is_err());
		assert_eq!(calls, 1);
	}
}
