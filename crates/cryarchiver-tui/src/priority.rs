//! The TUI's own threads, the loop that draws and the input reader, run above normal priority, so a key press gets a
//! core at once while encoders keep every core busy. Everything else keeps its priority: a thread they start for
//! other work restores normal priority first, since on Linux it would inherit a raised nice value.

pub use native::{raise_this_thread, restore_this_thread};

/// Opening a project starts the run's threads, which on Linux would inherit a raised nice value.
pub fn while_normal<T>(work: impl FnOnce() -> T) -> T {
	restore_this_thread();
	let result = work();
	raise_this_thread();
	result
}

#[cfg(windows)]
mod native {
	use windows::Win32::System::Threading::{
		GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_NORMAL,
	};

	pub fn raise_this_thread() {
		// SAFETY: the pseudo handle of the current thread is always valid and needs no closing.
		let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
	}

	pub fn restore_this_thread() {
		// SAFETY: as above.
		let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL) };
	}
}

/// A lower nice value needs privileges; without them the thread simply stays where it was.
#[cfg(target_os = "linux")]
mod native {
	use std::sync::OnceLock;

	const RAISE_BY: libc::c_int = 5;
	const HIGHEST: libc::c_int = -20;

	static ORIGINAL: OnceLock<libc::c_int> = OnceLock::new();

	fn this_thread() -> libc::id_t {
		// SAFETY: gettid has no preconditions.
		libc::id_t::try_from(unsafe { libc::gettid() }).unwrap_or_default()
	}

	fn set(nice: libc::c_int) {
		// SAFETY: setpriority only reads its integer arguments and affects this thread alone.
		unsafe { libc::setpriority(libc::PRIO_PROCESS, this_thread(), nice) };
	}

	/// The nice value the TUI found, before raising anything.
	fn original() -> libc::c_int {
		// SAFETY: getpriority only reads this thread's own value. -1 is also a valid nice value, so errno tells an
		// error apart; it is this thread's own.
		*ORIGINAL.get_or_init(|| unsafe {
			*libc::__errno_location() = 0;
			let nice = libc::getpriority(libc::PRIO_PROCESS, this_thread());
			if nice == -1 && *libc::__errno_location() != 0 {
				0
			} else {
				nice
			}
		})
	}

	pub fn raise_this_thread() {
		set((original() - RAISE_BY).max(HIGHEST));
	}

	pub fn restore_this_thread() {
		set(original());
	}
}

#[cfg(not(any(windows, target_os = "linux")))]
mod native {
	pub fn raise_this_thread() {}

	pub fn restore_this_thread() {}
}

#[cfg(test)]
mod tests {
	use std::thread;

	use super::*;

	#[cfg(windows)]
	fn this_thread_priority() -> i32 {
		use windows::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};

		// SAFETY: the pseudo handle of the current thread is always valid.
		unsafe { GetThreadPriority(GetCurrentThread()) }
	}

	#[cfg(target_os = "linux")]
	fn this_thread_priority() -> i32 {
		// SAFETY: getpriority only reads this thread's own nice value.
		-unsafe { libc::getpriority(libc::PRIO_PROCESS, libc::id_t::try_from(libc::gettid()).unwrap()) }
	}

	/// On Linux raising needs privileges, so it may leave the thread where it was, never below.
	#[cfg(any(windows, target_os = "linux"))]
	#[test]
	fn the_ui_thread_is_raised_and_back_to_normal_only_while_a_project_opens() {
		thread::spawn(|| {
			let normal = this_thread_priority();

			raise_this_thread();
			let raised = this_thread_priority();
			let during_open = while_normal(this_thread_priority);

			#[cfg(windows)]
			assert!(raised > normal, "{raised} vs {normal}");
			#[cfg(not(windows))]
			assert!(raised >= normal, "{raised} vs {normal}");
			assert_eq!(during_open, normal);
			assert_eq!(this_thread_priority(), raised);
		})
		.join()
		.unwrap();
	}

	#[cfg(any(windows, target_os = "linux"))]
	#[test]
	fn a_thread_started_by_the_raised_ui_thread_can_restore_normal_priority() {
		thread::spawn(|| {
			let normal = this_thread_priority();
			raise_this_thread();

			let started = thread::spawn(|| {
				restore_this_thread();
				this_thread_priority()
			})
			.join()
			.unwrap();
			assert_eq!(started, normal);
		})
		.join()
		.unwrap();
	}

	#[cfg(windows)]
	#[test]
	fn threads_started_by_a_raised_thread_keep_normal_priority_on_windows() {
		thread::spawn(|| {
			let normal = this_thread_priority();
			raise_this_thread();

			let started = thread::spawn(this_thread_priority).join().unwrap();
			assert_eq!(started, normal);
		})
		.join()
		.unwrap();
	}
}
