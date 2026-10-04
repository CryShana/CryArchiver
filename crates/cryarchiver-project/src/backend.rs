//! The real backend: the engine with ffmpeg, and the machine's load from the OS.

use std::sync::{Arc, Mutex, PoisonError};

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

impl Backend for EngineBackend {
	fn encoder(
		&self,
		image_threads: usize,
		memory: Arc<dyn MemoryGate>,
		quality: Arc<dyn QualityMemory>,
	) -> Arc<dyn Encode> {
		Arc::new(
			Engine::new(self.tools.clone())
				.with_image_threads(image_threads)
				.with_thread_budget(image_threads)
				.with_memory_gate(memory)
				.with_quality_memory(quality),
		)
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
