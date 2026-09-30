use std::sync::{Mutex, PoisonError};

use cryarchiver_engine::engine::MemoryGate;
use cryarchiver_project::memory::MemoryBudget;
use sysinfo::System;

#[derive(Default)]
pub struct SystemMemory {
	system: Mutex<System>,
	budget: MemoryBudget,
}

impl MemoryGate for SystemMemory {
	fn try_reserve(&self, bytes: u64) -> bool {
		let (available, total) = {
			let mut system = self.system.lock().unwrap_or_else(PoisonError::into_inner);
			system.refresh_memory();
			(system.available_memory(), system.total_memory())
		};

		self.budget.observe(available, total);
		self.budget.try_reserve(bytes)
	}

	fn release(&self, bytes: u64) {
		self.budget.release(bytes);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_small_job_is_admitted_and_released() {
		let gate = SystemMemory::default();

		assert!(gate.try_reserve(1 << 20));
		gate.release(1 << 20);
	}
}
