//! The performance limit for this process and every tool it runs (project layer spec §4).
//!
//! Windows puts the whole process into one job object on first use; the tools it starts inherit it, so the priority
//! class and the CPU rate reach running tools, and the in-process encoders, the moment they change. Linux renices
//! the process groups of running tools and starts new ones at low priority. It can't raise a priority back without
//! privileges, and it has no live CPU cap: the project gives new jobs fewer threads instead.

use std::sync::{Mutex, PoisonError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
	pub low_priority: bool,
	/// Percent of the whole machine.
	pub cpu_cap: Option<u8>,
}

/// Which parts of the last [`apply`] reached the processes that were already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
	pub priority_live: bool,
	pub cpu_cap_live: bool,
}

static CURRENT: Mutex<Limits> = Mutex::new(Limits {
	low_priority: false,
	cpu_cap: None,
});

pub fn current() -> Limits {
	*CURRENT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The first call must come before any tool starts: on Windows, tools started earlier stay outside the limit.
pub fn apply(limits: Limits) -> Applied {
	*CURRENT.lock().unwrap_or_else(PoisonError::into_inner) = limits;
	native::apply(limits)
}

/// Keeps a running tool's process group known to [`apply`] until it is dropped.
pub struct Tracked {
	#[cfg(target_os = "linux")]
	group: Option<i32>,
}

pub fn track(pid: Option<u32>) -> Tracked {
	#[cfg(target_os = "linux")]
	{
		let group = pid.and_then(|pid| i32::try_from(pid).ok());
		if let Some(group) = group {
			native::register(group);
		}
		Tracked { group }
	}

	#[cfg(not(target_os = "linux"))]
	{
		let _ = pid;
		Tracked {}
	}
}

#[cfg(target_os = "linux")]
impl Drop for Tracked {
	fn drop(&mut self) {
		if let Some(group) = self.group {
			native::unregister(group);
		}
	}
}

#[cfg(windows)]
mod native {
	use std::ffi::c_void;
	use std::sync::OnceLock;

	use windows::Win32::Foundation::HANDLE;
	use windows::Win32::System::JobObjects::{
		AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
		JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_PRIORITY_CLASS, JOBOBJECT_BASIC_LIMIT_INFORMATION,
		JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0,
		JobObjectBasicLimitInformation, JobObjectCpuRateControlInformation, SetInformationJobObject,
	};
	use windows::Win32::System::Threading::{GetCurrentProcess, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS};
	use windows::core::PCWSTR;

	use super::{Applied, Limits};

	static JOB: OnceLock<Option<usize>> = OnceLock::new();

	pub fn apply(limits: Limits) -> Applied {
		let Some(job) = job() else {
			return Applied {
				priority_live: false,
				cpu_cap_live: false,
			};
		};

		Applied {
			priority_live: set_priority(job, limits.low_priority).is_ok(),
			cpu_cap_live: set_cpu_cap(job, limits.cpu_cap).is_ok(),
		}
	}

	fn job() -> Option<HANDLE> {
		let address = JOB.get_or_init(|| {
			// SAFETY: no attributes and no name; the handle stays open for the life of the process.
			let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.ok()?;
			// SAFETY: both handles are valid; the pseudo handle of the current process needs no closing.
			unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) }.ok()?;
			Some(job.0.expose_provenance())
		});
		address.map(|address| HANDLE(std::ptr::with_exposed_provenance_mut::<c_void>(address)))
	}

	fn byte_size<T>() -> u32 {
		u32::try_from(size_of::<T>()).expect("job information structs are small")
	}

	// Clearing the limit flag would leave running processes at the idle class, so lifting sets the normal class.
	fn set_priority(job: HANDLE, low: bool) -> windows::core::Result<()> {
		let class = if low {
			IDLE_PRIORITY_CLASS
		} else {
			NORMAL_PRIORITY_CLASS
		};
		let info = JOBOBJECT_BASIC_LIMIT_INFORMATION {
			LimitFlags: JOB_OBJECT_LIMIT_PRIORITY_CLASS,
			PriorityClass: class.0,
			..JOBOBJECT_BASIC_LIMIT_INFORMATION::default()
		};

		// SAFETY: the pointer and size describe `info`, which outlives the call.
		unsafe {
			SetInformationJobObject(
				job,
				JobObjectBasicLimitInformation,
				(&raw const info).cast(),
				byte_size::<JOBOBJECT_BASIC_LIMIT_INFORMATION>(),
			)
		}
	}

	fn set_cpu_cap(job: HANDLE, cap: Option<u8>) -> windows::core::Result<()> {
		let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
			ControlFlags: match cap {
				Some(_) => JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
				None => JOB_OBJECT_CPU_RATE_CONTROL(0),
			},
			Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 {
				CpuRate: cap.map_or(0, |percent| u32::from(percent.clamp(1, 100)) * 100),
			},
		};

		// SAFETY: the pointer and size describe `info`, which outlives the call.
		unsafe {
			SetInformationJobObject(
				job,
				JobObjectCpuRateControlInformation,
				(&raw const info).cast(),
				byte_size::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>(),
			)
		}
	}
}

#[cfg(target_os = "linux")]
mod native {
	use std::sync::{Mutex, PoisonError};

	use super::{Applied, Limits};

	const LOW_NICE: libc::c_int = 19;
	const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
	const IOPRIO_CLASS_NONE: libc::c_int = 0;
	const IOPRIO_CLASS_IDLE: libc::c_int = 3;
	const IOPRIO_WHO_PROCESS: libc::c_int = 1;
	const IOPRIO_WHO_PGRP: libc::c_int = 2;

	static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

	pub fn register(group: i32) {
		GROUPS.lock().unwrap_or_else(PoisonError::into_inner).push(group);
	}

	pub fn unregister(group: i32) {
		GROUPS
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.retain(|known| *known != group);
	}

	pub fn apply(limits: Limits) -> Applied {
		let groups = GROUPS.lock().unwrap_or_else(PoisonError::into_inner).clone();
		let priority_live = groups
			.into_iter()
			.all(|group| set_priority(libc::PRIO_PGRP, IOPRIO_WHO_PGRP, group, limits.low_priority));

		Applied {
			priority_live,
			cpu_cap_live: limits.cpu_cap.is_none(),
		}
	}

	/// Runs in a freshly forked tool before `exec`: only async-signal-safe calls.
	pub fn lower_this_process() {
		set_priority(libc::PRIO_PROCESS, IOPRIO_WHO_PROCESS, 0, true);
	}

	fn set_priority(which: libc::__priority_which_t, who_kind: libc::c_int, who: i32, low: bool) -> bool {
		let (nice, class) = if low {
			(LOW_NICE, IOPRIO_CLASS_IDLE)
		} else {
			(0, IOPRIO_CLASS_NONE)
		};
		let id = libc::id_t::try_from(who).unwrap_or(0);

		// SAFETY: setpriority and ioprio_set only read their integer arguments.
		unsafe {
			let niced = libc::setpriority(which, id, nice) == 0;
			let io = libc::syscall(libc::SYS_ioprio_set, who_kind, who, class << IOPRIO_CLASS_SHIFT) == 0;
			niced && io
		}
	}
}

#[cfg(target_os = "linux")]
pub(crate) use native::lower_this_process;

#[cfg(not(any(windows, target_os = "linux")))]
mod native {
	use super::{Applied, Limits};

	pub fn apply(_limits: Limits) -> Applied {
		Applied {
			priority_live: false,
			cpu_cap_live: false,
		}
	}
}
