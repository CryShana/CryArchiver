//! Memory admission (project layer spec §8.2): a job starts only if its estimated peak fits in the available RAM
//! minus a reserve of the larger of 2 GB and 15 %. With nothing else admitted, a job always starts, so an estimate
//! larger than the machine runs alone instead of never.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use cryarchiver_engine::engine::MemoryGate;

const MIN_RESERVE: u64 = 2 << 30;
const RESERVE_PERCENT: u64 = 15;

#[derive(Debug, Default)]
pub struct MemoryBudget {
	available: AtomicU64,
	total: AtomicU64,
	reserved: Mutex<u64>,
}

impl MemoryBudget {
	pub fn observe(&self, available: u64, total: u64) {
		self.available.store(available, Ordering::SeqCst);
		self.total.store(total, Ordering::SeqCst);
	}

	pub fn reserved(&self) -> u64 {
		*self.reserved.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn reserve_floor(total: u64) -> u64 {
		MIN_RESERVE.max(total / 100 * RESERVE_PERCENT)
	}
}

impl MemoryGate for MemoryBudget {
	fn try_reserve(&self, bytes: u64) -> bool {
		let mut reserved = self.reserved.lock().unwrap_or_else(PoisonError::into_inner);
		let total = self.total.load(Ordering::SeqCst);
		let floor = Self::reserve_floor(total);
		let available = self.available.load(Ordering::SeqCst).saturating_sub(floor);
		let committable = total.saturating_sub(floor);

		let fits = bytes <= available && reserved.saturating_add(bytes) <= committable;
		if *reserved == 0 || fits {
			*reserved += bytes;
			return true;
		}

		false
	}

	fn release(&self, bytes: u64) {
		let mut reserved = self.reserved.lock().unwrap_or_else(PoisonError::into_inner);
		*reserved = reserved.saturating_sub(bytes);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const GIB: u64 = 1 << 30;

	#[test]
	fn a_job_fits_in_available_ram_minus_the_reserve() {
		let budget = MemoryBudget::default();
		budget.observe(6 * GIB, 16 * GIB);

		assert!(budget.try_reserve(GIB));
		assert!(!budget.try_reserve(4 * GIB));
		assert!(budget.try_reserve(3 * GIB));
		assert_eq!(budget.reserved(), 4 * GIB);
	}

	#[test]
	fn the_reserve_is_at_least_two_gigabytes_or_fifteen_percent() {
		assert_eq!(MemoryBudget::reserve_floor(8 * GIB), 2 * GIB);
		assert_eq!(MemoryBudget::reserve_floor(100 * GIB), 15 * GIB);
	}

	#[test]
	fn estimates_add_up_even_before_the_jobs_allocate() {
		let budget = MemoryBudget::default();
		budget.observe(30 * GIB, 32 * GIB);

		assert!(budget.try_reserve(20 * GIB));
		assert!(!budget.try_reserve(10 * GIB));
	}

	#[test]
	fn a_job_larger_than_the_machine_runs_alone() {
		let budget = MemoryBudget::default();
		budget.observe(4 * GIB, 8 * GIB);

		assert!(budget.try_reserve(20 * GIB));
		assert!(!budget.try_reserve(GIB / 2));

		budget.release(20 * GIB);
		assert!(budget.try_reserve(GIB / 2));
	}
}
