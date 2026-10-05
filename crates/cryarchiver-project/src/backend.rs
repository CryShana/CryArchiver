//! The real backend: the engine with ffmpeg, and the machine's load from the OS.

use std::sync::{Arc, Mutex, PoisonError};

use cryarchiver_engine::budget::cores;
use cryarchiver_engine::engine::{Encode, Engine, MemoryGate};
use cryarchiver_engine::quality::memory::QualityMemory;
use cryarchiver_engine::toolchain::{FfmpegBuild, Toolchain};
use sysinfo::System;

use crate::project::{Backend, SystemSample};

pub struct EngineBackend {
	tools: Toolchain,
	system: Mutex<System>,
}

impl EngineBackend {
	pub fn new(tools: Toolchain) -> Self {
		let mut system = System::new();
		system.refresh_cpu_usage();
		system.refresh_memory();

		Self {
			tools,
			system: Mutex::new(system),
		}
	}
}

/// The thread budget of every job when the performance limit can't be applied live and leaves fewer image threads
/// than cores: videos are then capped to the same share, encoder included.
fn video_threads(image_threads: usize, cores: usize) -> Option<usize> {
	(image_threads < cores).then_some(image_threads)
}

impl Backend for EngineBackend {
	fn encoder(
		&self,
		image_threads: usize,
		memory: Arc<dyn MemoryGate>,
		quality: Arc<dyn QualityMemory>,
	) -> Arc<dyn Encode> {
		let mut engine = Engine::new(self.tools.clone())
			.with_image_threads(image_threads)
			.with_thread_budget(image_threads)
			.with_memory_gate(memory)
			.with_quality_memory(quality);
		if let Some(threads) = video_threads(image_threads, cores()) {
			engine = engine.with_threads(threads);
		}
		Arc::new(engine)
	}

	fn toolchain(&self) -> String {
		let ffmpeg = match &self.tools.build {
			FfmpegBuild::Release(version) => format!("ffmpeg {version}"),
			FfmpegBuild::Git(revision) => format!("ffmpeg {revision}"),
		};

		match self.tools.svt_av1 {
			Some(version) => format!("{ffmpeg} · SVT-AV1 {version}"),
			None => ffmpeg,
		}
	}

	fn sample(&self) -> SystemSample {
		let mut system = self.system.lock().unwrap_or_else(PoisonError::into_inner);
		system.refresh_cpu_usage();
		system.refresh_memory();

		SystemSample {
			cpu_busy: system.global_cpu_usage() / 100.0,
			memory_available: system.available_memory(),
			memory_total: system.total_memory(),
		}
	}
}

#[cfg(test)]
mod tests {
	use cryarchiver_engine::video::plan::threads_for;

	use super::*;

	#[test]
	fn a_limit_that_leaves_fewer_image_threads_than_cores_caps_the_video_encoder_too() {
		assert_eq!(video_threads(8, 32), Some(8));
		assert_eq!(video_threads(1, 4), Some(1));
		assert_eq!(video_threads(32, 32), None, "no limit, or one the OS applies live");

		let capped = threads_for(video_threads(6, 8).unwrap());
		assert_eq!(
			(capped.tool, capped.svt_lp),
			(6, 2),
			"SVT-AV1 at a level whose CPU use fits 6 threads"
		);
	}
}
