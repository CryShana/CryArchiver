//! The thread budget the jobs of one engine share. Every running job holds tokens for the threads its tools may use;
//! a job's measuring may lease more for a while, but only tokens no other job holds, and it gives them back when the
//! lease ends. Holding never waits: a job always gets its own share, so the budget only ever limits the leases.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::task::Poll;

use crate::engine::MemoryGate;

#[derive(Debug)]
pub struct ThreadBudget {
	total: AtomicUsize,
	state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
	holders: usize,
	held: usize,
}

/// A running job's own tokens.
#[derive(Debug)]
pub struct Hold<'a> {
	budget: &'a ThreadBudget,
	tokens: usize,
}

/// Threads a holder may use beyond its own until the lease is dropped.
#[derive(Debug)]
pub struct Lease<'a> {
	budget: &'a ThreadBudget,
	extra: usize,
	pub threads: usize,
}

/// How many threads a job's measuring tools may use at once.
#[derive(Debug, Clone, Copy)]
pub enum Width<'a> {
	/// One tool at a time, on the caps of the job's plan.
	Serial,
	/// The job's own thread budget, which it never goes beyond.
	Fixed(usize),
	/// The job's fair share of the engine's budget, or all that the other jobs leave free when that is more.
	Shared(&'a Hold<'a>),
}

/// Where a measuring phase gets its threads, and the memory each tool run beside the first one needs.
#[derive(Clone, Copy)]
pub struct Lanes<'a> {
	pub width: Width<'a>,
	pub memory: Option<&'a dyn MemoryGate>,
	pub lane_bytes: u64,
}

/// The lanes of one measuring phase: how many tools run at once and each one's threads. Dropping it returns the
/// leased threads and the reserved memory.
pub struct Granted<'a> {
	pub lanes: usize,
	threads: usize,
	serial: bool,
	_lease: Option<Lease<'a>>,
	memory: Option<(&'a dyn MemoryGate, u64)>,
}

impl ThreadBudget {
	pub fn new(total: usize) -> Self {
		Self {
			total: AtomicUsize::new(total.max(1)),
			state: Mutex::new(State::default()),
		}
	}

	pub fn resize(&self, total: usize) {
		self.total.store(total.max(1), Ordering::SeqCst);
	}

	pub fn total(&self) -> usize {
		self.total.load(Ordering::SeqCst)
	}

	/// The tokens no job holds.
	pub fn free(&self) -> usize {
		self.total().saturating_sub(self.state().held)
	}

	pub fn hold(&self, tokens: usize) -> Hold<'_> {
		let mut state = self.state();
		state.holders += 1;
		state.held += tokens;
		Hold { budget: self, tokens }
	}

	fn state(&self) -> std::sync::MutexGuard<'_, State> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

impl Hold<'_> {
	/// The holder's fair share of the budget, or all the tokens the other holders leave free when that is more, held
	/// until the lease is dropped.
	pub fn lease(&self) -> Lease<'_> {
		let total = self.budget.total();
		let mut state = self.budget.state();
		let others = state.held.saturating_sub(self.tokens);
		let threads = (total / state.holders.max(1)).max(total.saturating_sub(others)).max(1);
		let extra = threads.saturating_sub(self.tokens);
		state.held += extra;

		Lease {
			budget: self.budget,
			extra,
			threads,
		}
	}
}

impl Drop for Hold<'_> {
	fn drop(&mut self) {
		let mut state = self.budget.state();
		state.holders = state.holders.saturating_sub(1);
		state.held = state.held.saturating_sub(self.tokens);
	}
}

impl Drop for Lease<'_> {
	fn drop(&mut self) {
		let mut state = self.budget.state();
		state.held = state.held.saturating_sub(self.extra);
	}
}

impl<'a> Lanes<'a> {
	pub const SERIAL: Lanes<'static> = Lanes {
		width: Width::Serial,
		memory: None,
		lane_bytes: 0,
	};

	/// Lanes for `count` tool runs: one for every `per_lane` threads of the width, at most `count`, and no more than
	/// the memory gate admits beside the first.
	pub fn grant(&self, count: usize, per_lane: usize) -> Granted<'a> {
		let (threads, lease) = match self.width {
			Width::Serial => (0, None),
			Width::Fixed(threads) => (threads, None),
			Width::Shared(hold) => {
				let lease = hold.lease();
				(lease.threads, Some(lease))
			}
		};

		let (mut lanes, _) = split(threads, count, per_lane);
		let mut memory = None;
		while lanes > 1 {
			let bytes = self.lane_bytes * (lanes as u64 - 1);
			match self.memory {
				None => break,
				Some(gate) if gate.try_reserve(bytes) => {
					memory = Some((gate, bytes));
					break;
				}
				Some(_) => lanes -= 1,
			}
		}

		Granted {
			lanes,
			threads: (threads / lanes).max(1),
			serial: matches!(self.width, Width::Serial),
			_lease: lease,
			memory,
		}
	}
}

impl Granted<'_> {
	/// Each lane's tool threads, when the phase runs more than one tool at once.
	pub fn lane_threads(&self) -> Option<usize> {
		(self.lanes > 1).then_some(self.threads)
	}

	/// Each lane's tool threads, unless the tools run on the caps of the job's plan.
	pub fn threads(&self) -> Option<usize> {
		(!self.serial).then_some(self.threads)
	}
}

impl Drop for Granted<'_> {
	fn drop(&mut self) {
		if let Some((gate, bytes)) = self.memory {
			gate.release(bytes);
		}
	}
}

pub fn cores() -> usize {
	std::thread::available_parallelism().map_or(1, usize::from)
}

/// How many of `count` tool runs fit at once in `threads`, at `per_lane` each and at least one, and each one's
/// threads.
pub fn split(threads: usize, count: usize, per_lane: usize) -> (usize, usize) {
	let lanes = (threads / per_lane.max(1)).clamp(1, count.max(1));
	(lanes, (threads / lanes).max(1))
}

/// Runs `work` on the items `0..count`, at most `lanes` at once, each lane taking the next item as it finishes one.
/// Returns the results in item order; the first error ends the other lanes.
pub async fn in_lanes<T, E, Run>(count: usize, lanes: usize, work: impl Fn(usize) -> Run) -> Result<Vec<T>, E>
where
	Run: Future<Output = Result<T, E>>,
{
	let (next, work) = (&AtomicUsize::new(0), &work);
	let lane = move || async move {
		let mut done = Vec::new();
		loop {
			let index = next.fetch_add(1, Ordering::SeqCst);
			if index >= count {
				return Ok::<_, E>(done);
			}
			done.push((index, work(index).await?));
		}
	};

	let mut running: Vec<_> = (0..lanes.clamp(1, count.max(1))).map(|_| Box::pin(lane())).collect();
	let mut results = Vec::with_capacity(count);
	std::future::poll_fn(|cx| {
		let mut index = 0;
		while index < running.len() {
			match running[index].as_mut().poll(cx) {
				Poll::Ready(Ok(done)) => {
					results.extend(done);
					drop(running.swap_remove(index));
				}
				Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
				Poll::Pending => index += 1,
			}
		}

		if running.is_empty() {
			Poll::Ready(Ok(()))
		} else {
			Poll::Pending
		}
	})
	.await?;

	results.sort_by_key(|(index, _)| *index);
	Ok(results.into_iter().map(|(_, value)| value).collect())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_lone_holder_leases_the_whole_budget_and_gives_it_back() {
		let budget = ThreadBudget::new(8);
		let hold = budget.hold(2);

		let lease = hold.lease();

		assert_eq!(lease.threads, 8);
		assert_eq!(budget.free(), 0);
		drop(lease);
		assert_eq!(budget.free(), 6);
		drop(hold);
		assert_eq!(budget.free(), 8);
	}

	#[test]
	fn a_lease_takes_only_what_other_holders_leave_free_or_its_fair_share() {
		let budget = ThreadBudget::new(8);
		let image = budget.hold(2);
		let video = budget.hold(4);

		assert_eq!(video.lease().threads, 6);
		drop(image);

		let busy = budget.hold(8);
		assert_eq!(video.lease().threads, 4, "half of the budget between two holders");
		drop(busy);
	}

	#[test]
	fn two_leases_never_share_the_same_free_tokens() {
		let budget = ThreadBudget::new(8);
		let first = budget.hold(2);
		let second = budget.hold(2);

		let leased = first.lease();
		let other = second.lease();

		assert_eq!(leased.threads, 6);
		assert_eq!(other.threads, 4, "only its fair share is left");
		drop((leased, other));
		assert_eq!(budget.free(), 4);
	}

	#[test]
	fn a_resized_budget_reaches_the_next_lease() {
		let budget = ThreadBudget::new(8);
		let hold = budget.hold(1);

		budget.resize(3);

		assert_eq!(hold.lease().threads, 3);
	}

	#[test]
	fn tool_runs_split_the_threads_at_most_one_lane_each() {
		assert_eq!(split(8, 4, 2), (4, 2));
		assert_eq!(split(8, 12, 2), (4, 2));
		assert_eq!(split(32, 4, 2), (4, 8));
		assert_eq!(split(9, 4, 2), (4, 2));
		assert_eq!(split(3, 4, 2), (1, 3));
		assert_eq!(split(1, 4, 2), (1, 1));
		assert_eq!(split(0, 4, 2), (1, 1));
		assert_eq!(split(8, 0, 2), (1, 8));
		assert_eq!(split(8, 12, 1), (8, 1));
		assert_eq!(split(32, 8, 1), (8, 4));
		assert_eq!(split(8, 4, 0), (4, 2));
	}

	#[derive(Default)]
	struct Gate {
		allowed: u64,
		reserved: AtomicUsize,
	}

	impl MemoryGate for Gate {
		fn try_reserve(&self, bytes: u64) -> bool {
			let fits = bytes <= self.allowed;
			if fits {
				self.reserved
					.fetch_add(usize::try_from(bytes).unwrap(), Ordering::SeqCst);
			}
			fits
		}

		fn release(&self, bytes: u64) {
			self.reserved
				.fetch_sub(usize::try_from(bytes).unwrap(), Ordering::SeqCst);
		}
	}

	#[test]
	fn lanes_beside_the_first_need_their_memory() {
		let gate = Gate {
			allowed: 200,
			..Gate::default()
		};
		let lanes = Lanes {
			width: Width::Fixed(8),
			memory: Some(&gate),
			lane_bytes: 100,
		};

		let granted = lanes.grant(4, 2);

		assert_eq!((granted.lanes, granted.lane_threads()), (3, Some(2)));
		assert_eq!(gate.reserved.load(Ordering::SeqCst), 200);
		drop(granted);
		assert_eq!(gate.reserved.load(Ordering::SeqCst), 0);
	}

	#[test]
	fn one_lane_keeps_the_plan_caps_and_serial_lanes_have_no_threads_of_their_own() {
		let budget = ThreadBudget::new(3);
		let hold = budget.hold(1);
		let shared = Lanes {
			width: Width::Shared(&hold),
			memory: None,
			lane_bytes: 0,
		};

		let granted = shared.grant(4, 2);

		assert_eq!(
			(granted.lanes, granted.lane_threads(), granted.threads()),
			(1, None, Some(3))
		);
		assert_eq!(budget.free(), 0, "the lease lasts as long as the lanes");
		drop(granted);
		assert_eq!(budget.free(), 2);

		let serial = Lanes::SERIAL.grant(4, 2);
		assert_eq!((serial.lanes, serial.lane_threads(), serial.threads()), (1, None, None));
	}

	#[tokio::test]
	async fn lanes_return_every_result_in_order_and_stop_at_the_first_error() {
		let (running, most) = (&AtomicUsize::new(0), &AtomicUsize::new(0));
		let work = move |index: usize| async move {
			let now = running.fetch_add(1, Ordering::SeqCst) + 1;
			most.fetch_max(now, Ordering::SeqCst);
			tokio::time::sleep(std::time::Duration::from_millis(5 * (7 - index as u64))).await;
			running.fetch_sub(1, Ordering::SeqCst);
			Ok::<_, String>(index * 10)
		};

		let all = in_lanes(7, 3, work).await.unwrap();

		assert_eq!(all, [0, 10, 20, 30, 40, 50, 60]);
		assert_eq!(most.load(Ordering::SeqCst), 3);

		let failed = in_lanes(5, 2, |index: usize| async move {
			if index == 1 {
				Err(format!("item {index}"))
			} else {
				Ok(index)
			}
		})
		.await;
		assert_eq!(failed, Err("item 1".to_owned()));
		assert_eq!(
			in_lanes(0, 4, |index: usize| async move { Ok::<_, ()>(index) }).await,
			Ok(Vec::new())
		);
	}
}
