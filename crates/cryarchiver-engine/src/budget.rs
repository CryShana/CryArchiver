//! The thread budget the jobs of one engine share. Every running job holds tokens for the threads its tools may use;
//! holding never waits, so a job always gets its own tokens. A job's measuring leases threads for a phase: its fair
//! share of the budget, capped at its own tokens, widened into the tokens no job holds. Whenever a job that starts
//! later puts the budget over its size, the measuring gives the widened part back lane by lane, before each lane's
//! next sample or segment, and all of it when the phase ends. A job without a thread budget of its own holds the
//! whole budget while it runs a stage on uncapped tools, since they may use the whole machine.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
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

/// Threads a holder may use for a while: `extra` of them beyond its own tokens, held until given back.
#[derive(Debug)]
pub struct Lease<'a> {
	budget: &'a ThreadBudget,
	extra: AtomicUsize,
	pub threads: usize,
}

/// How many threads a job's measuring tools may use at once.
#[derive(Debug, Clone, Copy)]
pub enum Width<'a> {
	/// One tool at a time, on the caps of the job's plan.
	Serial,
	/// The job's own thread budget, which it never goes beyond.
	Fixed(usize),
	/// The job's fair share of the engine's budget, capped at its own tokens, widened into the tokens no job holds.
	Shared(&'a Hold<'a>),
}

/// Where a measuring phase gets its threads, and the memory each tool run beside the first one needs.
#[derive(Clone, Copy)]
pub struct Lanes<'a> {
	pub width: Width<'a>,
	pub memory: Option<&'a dyn MemoryGate>,
	pub lane_bytes: u64,
}

/// The lanes of one measuring phase. Each lane asks `next` before it takes its next item. Dropping it returns the
/// leased threads and the reserved memory.
pub struct Granted<'a> {
	pub lanes: usize,
	/// Each lane's threads; none for serial lanes.
	width: AtomicUsize,
	plan_caps: AtomicBool,
	active: AtomicUsize,
	lease: Option<Lease<'a>>,
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

	/// The tokens the jobs hold, leases included.
	pub fn in_use(&self) -> usize {
		self.state().held
	}

	/// The tokens no job holds.
	pub fn free(&self) -> usize {
		self.total().saturating_sub(self.in_use())
	}

	pub fn hold(&self, tokens: usize) -> Hold<'_> {
		let mut state = self.state();
		state.holders += 1;
		state.held += tokens;
		Hold { budget: self, tokens }
	}

	fn state(&self) -> MutexGuard<'_, State> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

impl<'a> Hold<'a> {
	/// The holder's fair share of the budget, never more than its own tokens, or all the tokens the other holders
	/// leave free when that is more. The part beyond its own tokens is held until the lease is dropped, so a lease
	/// never puts the budget over its size.
	pub fn lease(&self) -> Lease<'a> {
		let total = self.budget.total();
		let mut state = self.budget.state();
		let others = state.held.saturating_sub(self.tokens);
		let fair = (total / state.holders.max(1)).min(self.tokens);
		let threads = fair.max(total.saturating_sub(others)).max(1);
		self.leased(&mut state, threads)
	}

	/// Holds the whole budget until the lease is dropped, for a stage whose tools run without caps of their own and
	/// may use the whole machine: no other job's measuring then counts those threads as free.
	pub fn raise_to_total(&self) -> Lease<'a> {
		let mut state = self.budget.state();
		self.leased(&mut state, self.budget.total())
	}

	fn leased(&self, state: &mut State, threads: usize) -> Lease<'a> {
		let extra = threads.saturating_sub(self.tokens);
		state.held += extra;

		Lease {
			budget: self.budget,
			extra: AtomicUsize::new(extra),
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
		state.held = state.held.saturating_sub(self.extra.load(Ordering::SeqCst));
	}
}

impl<'a> Lanes<'a> {
	pub const SERIAL: Lanes<'static> = Lanes {
		width: Width::Serial,
		memory: None,
		lane_bytes: 0,
	};

	/// Holds the whole budget for a stage on the plan's caps when they are none: a job without a thread budget of its
	/// own. A job with one stays within it.
	pub fn uncapped_stage(&self) -> Option<Lease<'a>> {
		match self.width {
			Width::Shared(hold) => Some(hold.raise_to_total()),
			Width::Serial | Width::Fixed(_) => None,
		}
	}

	/// Lanes for `count` tool runs: one for every `per_lane` threads of the width once it reaches `from` threads, at
	/// most `count`, and no more than the memory gate admits beside the first.
	pub fn grant(&self, count: usize, per_lane: usize, from: usize) -> Granted<'a> {
		let (threads, lease, whole) = match self.width {
			Width::Serial => (0, None, true),
			Width::Fixed(threads) => (threads, None, true),
			Width::Shared(hold) => {
				let lease = hold.lease();
				let whole = lease.threads >= hold.budget.total();
				(lease.threads, Some(lease), whole)
			}
		};

		let mut lanes = if threads >= from {
			split(threads, count, per_lane)
		} else {
			1
		};
		let memory = self.memory.and_then(|gate| {
			while lanes > 1 {
				let bytes = self.lane_bytes * (lanes as u64 - 1);
				if gate.try_reserve(bytes) {
					return Some((gate, bytes));
				}
				lanes -= 1;
			}
			None
		});

		Granted {
			lanes,
			width: AtomicUsize::new(if threads == 0 { 0 } else { (threads / lanes).max(1) }),
			plan_caps: AtomicBool::new(lanes == 1 && whole),
			active: AtomicUsize::new(lanes),
			lease,
			memory,
		}
	}
}

impl Granted<'_> {
	/// The threads of a lane's next item, or `None` when the lane retires. While the budget is over its size, a lane
	/// gives the leased threads it runs on back instead of taking another item, the last lane narrowing to what it can
	/// still give back; a phase without leased threads, which then can't help, carries on.
	pub fn next(&self) -> Option<usize> {
		let width = self.width.load(Ordering::SeqCst);
		let Some(lease) = &self.lease else {
			return Some(width);
		};

		let total = lease.budget.total();
		let mut state = lease.budget.state();
		let over = state.held.saturating_sub(total);
		let extra = lease.extra.load(Ordering::SeqCst);
		if over == 0 || extra == 0 {
			return Some(width);
		}

		let retiring = self.active.load(Ordering::SeqCst) > 1;
		let given = if retiring {
			width.min(extra)
		} else {
			over.min(extra).min(width.saturating_sub(1))
		};
		lease.extra.store(extra - given, Ordering::SeqCst);
		state.held -= given;

		if retiring {
			self.active.fetch_sub(1, Ordering::SeqCst);
			return None;
		}

		if given > 0 {
			self.width.store(width - given, Ordering::SeqCst);
			self.plan_caps.store(false, Ordering::SeqCst);
		}
		Some(width - given)
	}

	/// Whether one tool at a time may run on the caps of the job's plan: a single lane on the job's whole budget.
	pub fn plan_caps(&self) -> bool {
		self.plan_caps.load(Ordering::SeqCst)
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

/// How many of `count` tool runs fit at once in `threads`, at `per_lane` each and at least one.
pub fn split(threads: usize, count: usize, per_lane: usize) -> usize {
	(threads / per_lane.max(1)).clamp(1, count.max(1))
}

/// Runs `work` on the items `0..count`, at most `lanes` at once. Before each item a lane asks `next` for the item's
/// threads, and retires when it gets none; at least one lane always keeps going. Returns the results in item order;
/// the first error ends the other lanes.
pub async fn in_lanes<T, E, Run>(
	count: usize,
	lanes: usize,
	next: impl Fn() -> Option<usize>,
	work: impl Fn(usize, usize) -> Run,
) -> Result<Vec<T>, E>
where
	Run: Future<Output = Result<T, E>>,
{
	let (taken, next, work) = (&AtomicUsize::new(0), &next, &work);
	let lane = move || async move {
		let mut done = Vec::new();
		while let Some(threads) = next() {
			let index = taken.fetch_add(1, Ordering::SeqCst);
			if index >= count {
				break;
			}
			done.push((index, work(index, threads).await?));
		}
		Ok::<_, E>(done)
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

	fn shared<'a>(hold: &'a Hold<'a>) -> Lanes<'a> {
		Lanes {
			width: Width::Shared(hold),
			..Lanes::SERIAL
		}
	}

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
	fn a_lease_takes_only_what_other_holders_leave_free_or_its_fair_share_of_its_own() {
		let budget = ThreadBudget::new(8);
		let image = budget.hold(2);
		let video = budget.hold(4);

		assert_eq!(video.lease().threads, 6);
		drop(image);

		let busy = budget.hold(8);
		assert_eq!(video.lease().threads, 4, "half of the budget between two holders");
		drop((busy, video));

		let whole = budget.hold(8);
		let image = budget.hold(8);
		assert_eq!(
			whole.lease().threads,
			4,
			"a whole-budget holder narrows to its fair share"
		);
		drop((whole, image));
	}

	#[test]
	fn two_videos_of_four_never_lease_the_budget_over_its_size() {
		let budget = ThreadBudget::new(32);
		let first = budget.hold(8);
		let second = budget.hold(8);

		let leased = first.lease();
		assert!(budget.in_use() <= budget.total());
		let other = second.lease();
		assert!(budget.in_use() <= budget.total());

		assert_eq!((leased.threads, other.threads), (24, 8));
		drop((leased, other));
		assert_eq!(budget.in_use(), 16);
	}

	#[test]
	fn lanes_never_borrow_on_top_of_another_jobs_uncapped_stage() {
		let budget = ThreadBudget::new(32);
		let encoding = budget.hold(16);
		let measuring = budget.hold(16);

		let raised = shared(&encoding).uncapped_stage();
		let during = shared(&measuring).grant(4, 4, 16);
		let lanes = during.lanes;
		let threads = (0..lanes).map(|_| during.next().unwrap()).sum::<usize>();
		drop(during);

		assert_eq!(
			threads, 16,
			"two jobs of -j 2 measure within their share of 16 during the other's encode"
		);
		assert_eq!(lanes, 4);
		drop(raised);
		assert_eq!(budget.in_use(), 32);
		drop(encoding);

		let alone = shared(&measuring).grant(4, 4, 16);
		let threads = (0..alone.lanes).map(|_| alone.next().unwrap()).sum::<usize>();
		assert_eq!(threads, 32, "the other half is borrowed once the other job ends");
		drop(alone);

		let fixed = Lanes {
			width: Width::Fixed(8),
			..Lanes::SERIAL
		};
		assert!(
			fixed.uncapped_stage().is_none(),
			"a budgeted job's stages stay within its budget"
		);
		assert_eq!(budget.in_use(), 16);
	}

	#[test]
	fn with_two_of_four_jobs_left_measuring_borrows_only_after_the_other_encode() {
		let budget = ThreadBudget::new(32);
		let encoding = budget.hold(8);
		let measuring = budget.hold(8);

		let raised = shared(&encoding).uncapped_stage();
		let during = shared(&measuring).grant(4, 4, 16);

		assert_eq!(
			(during.lanes, during.next()),
			(1, Some(8)),
			"-j 4 with two videos left: no borrowing on top of the other's encode"
		);
		drop((during, raised));

		let after = shared(&measuring).grant(4, 4, 16);
		assert_eq!(
			(after.lanes, after.next()),
			(4, Some(6)),
			"24 free once the encode ends"
		);
	}

	#[test]
	fn a_job_starting_during_a_lease_makes_the_lanes_give_their_threads_back() {
		let budget = ThreadBudget::new(32);
		let first = budget.hold(16);
		let granted = shared(&first).grant(4, 4, 16);
		assert_eq!((granted.lanes, budget.in_use()), (4, 32));
		assert_eq!(granted.next(), Some(8));

		let second = budget.hold(16);
		assert_eq!(budget.in_use(), 48);

		let mut lanes = Vec::new();
		for _ in 0..4 {
			let lane = granted.next();
			if lane.is_some() {
				assert!(budget.in_use() <= budget.total(), "no lane goes on over the budget");
			}
			lanes.push(lane);
		}

		assert_eq!(lanes, [None, None, Some(8), Some(8)]);
		assert_eq!(budget.in_use(), 32);
		drop(granted);
		assert_eq!(budget.in_use(), 32);
		drop(second);
		assert_eq!(budget.in_use(), 16);
	}

	#[test]
	fn the_last_lane_narrows_to_what_it_can_give_back() {
		let budget = ThreadBudget::new(8);
		let own = budget.hold(2);
		let granted = shared(&own).grant(1, 4, 0);
		assert_eq!((granted.lanes, granted.plan_caps()), (1, true));

		let later = budget.hold(4);

		assert_eq!(granted.next(), Some(4));
		assert!(budget.in_use() <= budget.total());
		assert!(!granted.plan_caps(), "a narrowed lane runs on its own caps");
		drop(later);
	}

	#[test]
	fn a_phase_without_leased_threads_carries_on_over_the_budget() {
		let budget = ThreadBudget::new(8);
		let own = budget.hold(8);
		let crowded = budget.hold(8);
		let granted = shared(&own).grant(8, 2, 0);

		assert_eq!(granted.lanes, 2);
		assert_eq!(
			(granted.next(), granted.next(), granted.next()),
			(Some(2), Some(2), Some(2))
		);
		drop(crowded);
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
		assert_eq!(split(8, 4, 2), 4);
		assert_eq!(split(8, 12, 2), 4);
		assert_eq!(split(32, 4, 4), 4);
		assert_eq!(split(32, 12, 4), 8);
		assert_eq!(split(3, 4, 2), 1);
		assert_eq!(split(0, 4, 2), 1);
		assert_eq!(split(8, 0, 2), 1);
		assert_eq!(split(8, 4, 0), 4);
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

		let granted = lanes.grant(4, 2, 0);

		assert_eq!(
			(granted.lanes, granted.next(), granted.plan_caps()),
			(3, Some(2), false)
		);
		assert_eq!(gate.reserved.load(Ordering::SeqCst), 200);
		drop(granted);
		assert_eq!(gate.reserved.load(Ordering::SeqCst), 0);
	}

	#[test]
	fn a_narrow_width_runs_one_lane_and_only_a_whole_budget_keeps_the_plan_caps() {
		let budget = ThreadBudget::new(8);
		let hold = budget.hold(1);

		let alone = shared(&hold).grant(4, 4, 16);
		assert_eq!((alone.lanes, alone.next(), alone.plan_caps()), (1, Some(8), true));
		drop(alone);

		let crowded = budget.hold(6);
		let narrowed = shared(&hold).grant(4, 4, 16);
		assert_eq!(
			(narrowed.lanes, narrowed.next(), narrowed.plan_caps()),
			(1, Some(2), false)
		);
		drop((narrowed, crowded));

		let fixed = Lanes {
			width: Width::Fixed(8),
			..Lanes::SERIAL
		}
		.grant(4, 4, 16);
		assert_eq!((fixed.lanes, fixed.plan_caps()), (1, true));

		let serial = Lanes::SERIAL.grant(4, 2, 0);
		assert_eq!((serial.lanes, serial.next(), serial.plan_caps()), (1, Some(0), true));
	}

	#[tokio::test]
	async fn lanes_return_every_result_in_order_and_stop_at_the_first_error() {
		let (running, most) = (&AtomicUsize::new(0), &AtomicUsize::new(0));
		let work = move |index: usize, _| async move {
			let now = running.fetch_add(1, Ordering::SeqCst) + 1;
			most.fetch_max(now, Ordering::SeqCst);
			tokio::time::sleep(std::time::Duration::from_millis(5 * (7 - index as u64))).await;
			running.fetch_sub(1, Ordering::SeqCst);
			Ok::<_, String>(index * 10)
		};

		let all = in_lanes(7, 3, || Some(1), work).await.unwrap();

		assert_eq!(all, [0, 10, 20, 30, 40, 50, 60]);
		assert_eq!(most.load(Ordering::SeqCst), 3);

		let failed = in_lanes(
			5,
			2,
			|| Some(1),
			|index: usize, _| async move {
				if index == 1 {
					Err(format!("item {index}"))
				} else {
					Ok(index)
				}
			},
		)
		.await;
		assert_eq!(failed, Err("item 1".to_owned()));
		assert_eq!(
			in_lanes(0, 4, || Some(1), |index: usize, _| async move { Ok::<_, ()>(index) }).await,
			Ok(Vec::new())
		);
	}

	#[tokio::test]
	async fn a_retired_lane_leaves_its_items_to_the_others() {
		let budget = ThreadBudget::new(8);
		let own = budget.hold(2);
		let granted = shared(&own).grant(6, 2, 0);
		assert_eq!(granted.lanes, 4);
		let (granted, budget) = (&granted, &budget);
		let starts = &Mutex::new(Vec::new());

		let later = &Mutex::new(None);

		let done = in_lanes(
			6,
			granted.lanes,
			|| granted.next(),
			move |index, threads| async move {
				starts.lock().unwrap().push((index, threads, budget.in_use()));
				if index == 0 {
					*later.lock().unwrap() = Some(budget.hold(4));
				}
				tokio::task::yield_now().await;
				Ok::<_, ()>(index)
			},
		)
		.await
		.unwrap();

		assert_eq!(done, [0, 1, 2, 3, 4, 5]);
		let starts = starts.lock().unwrap();
		assert!(starts.iter().all(|(_, _, in_use)| *in_use <= 8), "{starts:?}");
		assert_eq!(budget.in_use(), 8, "two retired lanes gave their threads back");
		drop(later.lock().unwrap().take());
	}
}
