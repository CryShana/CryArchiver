//! The performance limit for this process and every tool it runs (project layer spec §4).
//!
//! Windows puts the whole process into one job object on first use; the tools it starts inherit it, so the priority
//! class and the CPU rate reach running tools, and the in-process encoders, the moment they change. Linux renices
//! the process groups of running tools and starts new ones at low priority. It can't raise a priority back without
//! privileges, and it has no live CPU cap: the project gives new jobs fewer threads instead.

use std::io;
use std::process::{Child, Command};
use std::sync::{Mutex, PoisonError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum Priority {
	#[default]
	Normal,
	#[cfg_attr(feature = "clap", value(name = "low"))]
	BelowNormal,
	Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
	pub priority: Priority,
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
	priority: Priority::Normal,
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

/// Starts a program that isn't one of our tools, such as the file manager, outside the limit where the system
/// allows that: on Windows, out of the process's job object, falling back to a plain start when a job the process
/// was started in forbids leaving it.
pub fn spawn_outside_limits(command: &mut Command) -> io::Result<Child> {
	native::spawn_outside(command)
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

impl Drop for Tracked {
	fn drop(&mut self) {
		#[cfg(target_os = "linux")]
		if let Some(group) = self.group {
			native::unregister(group);
		}
	}
}

#[cfg(windows)]
mod native {
	use std::ffi::c_void;
	use std::io;
	use std::os::windows::process::CommandExt;
	use std::process::{Child, Command};
	use std::sync::OnceLock;

	use windows::Win32::Foundation::{CloseHandle, HANDLE};
	use windows::Win32::System::JobObjects::{
		AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
		JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
		JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
		JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
		JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation, SetInformationJobObject,
	};
	use windows::Win32::System::Threading::{
		BELOW_NORMAL_PRIORITY_CLASS, CREATE_BREAKAWAY_FROM_JOB, GetCurrentProcess, IDLE_PRIORITY_CLASS,
		NORMAL_PRIORITY_CLASS,
	};
	use windows::core::PCWSTR;

	use super::{Applied, Limits, Priority};

	static JOB: OnceLock<Option<usize>> = OnceLock::new();

	pub fn spawn_outside(command: &mut Command) -> io::Result<Child> {
		match command.creation_flags(CREATE_BREAKAWAY_FROM_JOB.0).spawn() {
			Err(err) if err.kind() == io::ErrorKind::PermissionDenied => command.creation_flags(0).spawn(),
			result => result,
		}
	}

	pub fn apply(limits: Limits) -> Applied {
		let Some(job) = job() else {
			return Applied {
				priority_live: false,
				cpu_cap_live: false,
			};
		};

		Applied {
			priority_live: set_priority(job, limits.priority).is_ok(),
			cpu_cap_live: set_cpu_cap(job, limits.cpu_cap).is_ok(),
		}
	}

	fn job() -> Option<HANDLE> {
		let address = JOB.get_or_init(|| {
			// SAFETY: no attributes and no name; the handle stays open for the life of the process.
			let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.ok()?;
			// SAFETY: both handles are valid; the pseudo handle of the current process needs no closing.
			if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) }.is_err() {
				// SAFETY: the job was created above and nothing else holds it.
				let _ = unsafe { CloseHandle(job) };
				return None;
			}

			Some(job.0.expose_provenance())
		});
		address.map(|address| HANDLE(std::ptr::with_exposed_provenance_mut::<c_void>(address)))
	}

	fn byte_size<T>() -> u32 {
		u32::try_from(size_of::<T>()).expect("job information structs are small")
	}

	// Clearing the limit flag would leave running processes at the idle class, so lifting sets the normal class. The
	// limits are set as a whole, so each call keeps leaving the job allowed: only for children started with
	// CREATE_BREAKAWAY_FROM_JOB, which our tools never are.
	fn set_priority(job: HANDLE, priority: Priority) -> windows::core::Result<()> {
		let class = match priority {
			Priority::Normal => NORMAL_PRIORITY_CLASS,
			Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
			Priority::Idle => IDLE_PRIORITY_CLASS,
		};
		let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
			BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
				LimitFlags: JOB_OBJECT_LIMIT_PRIORITY_CLASS | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
				PriorityClass: class.0,
				..JOBOBJECT_BASIC_LIMIT_INFORMATION::default()
			},
			..JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default()
		};

		// SAFETY: the pointer and size describe `info`, which outlives the call.
		unsafe {
			SetInformationJobObject(
				job,
				JobObjectExtendedLimitInformation,
				(&raw const info).cast(),
				byte_size::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
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
	use std::io;
	use std::process::{Child, Command};
	use std::sync::{Mutex, PoisonError};

	use super::{Applied, Limits, Priority};

	const LOW_NICE: libc::c_int = 19;
	const BELOW_NORMAL_NICE: libc::c_int = 10;
	const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
	const IOPRIO_CLASS_NONE: libc::c_int = 0;
	const IOPRIO_CLASS_BEST_EFFORT: libc::c_int = 2;
	const IOPRIO_CLASS_IDLE: libc::c_int = 3;
	const IOPRIO_LOWEST_LEVEL: libc::c_int = 7;
	const IOPRIO_WHO_PROCESS: libc::c_int = 1;
	const IOPRIO_WHO_PGRP: libc::c_int = 2;

	static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

	pub fn spawn_outside(command: &mut Command) -> io::Result<Child> {
		command.spawn()
	}

	pub fn register(group: i32) {
		GROUPS.lock().unwrap_or_else(PoisonError::into_inner).push(group);
	}

	pub fn unregister(group: i32) {
		let mut groups = GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
		if let Some(index) = groups.iter().position(|known| *known == group) {
			groups.swap_remove(index);
		}
	}

	#[derive(Clone, Copy)]
	enum Who {
		Group(i32),
		ThisProcess,
	}

	pub fn apply(limits: Limits) -> Applied {
		let groups = GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
		let failed = groups
			.iter()
			.filter(|&&group| !set_priority(Who::Group(group), limits.priority))
			.count();
		drop(groups);

		Applied {
			priority_live: failed == 0,
			cpu_cap_live: limits.cpu_cap.is_none(),
		}
	}

	/// Runs in a freshly forked tool before `exec`: only async-signal-safe calls.
	pub fn lower_this_process(priority: Priority) {
		set_priority(Who::ThisProcess, priority);
	}

	fn set_priority(who: Who, priority: Priority) -> bool {
		let (nice, io) = match priority {
			Priority::Normal => (0, IOPRIO_CLASS_NONE << IOPRIO_CLASS_SHIFT),
			Priority::BelowNormal => (
				BELOW_NORMAL_NICE,
				(IOPRIO_CLASS_BEST_EFFORT << IOPRIO_CLASS_SHIFT) | IOPRIO_LOWEST_LEVEL,
			),
			Priority::Idle => (LOW_NICE, IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT),
		};
		let (who_kind, id) = match who {
			Who::Group(group) => (IOPRIO_WHO_PGRP, group),
			Who::ThisProcess => (IOPRIO_WHO_PROCESS, 0),
		};
		let unsigned_id = libc::id_t::try_from(id).unwrap_or(0);

		// SAFETY: setpriority and ioprio_set only read their integer arguments.
		unsafe {
			// Each call names its `which` constant itself: glibc and musl give setpriority different types for it.
			let niced = match who {
				Who::Group(_) => libc::setpriority(libc::PRIO_PGRP, unsigned_id, nice),
				Who::ThisProcess => libc::setpriority(libc::PRIO_PROCESS, unsigned_id, nice),
			} == 0;
			let io_set = libc::syscall(libc::SYS_ioprio_set, who_kind, id, io) == 0;
			niced && io_set
		}
	}
}

#[cfg(target_os = "linux")]
pub(crate) use native::lower_this_process;

#[cfg(not(any(windows, target_os = "linux")))]
mod native {
	use std::io;
	use std::process::{Child, Command};

	use super::{Applied, Limits};

	pub fn spawn_outside(command: &mut Command) -> io::Result<Child> {
		command.spawn()
	}

	pub fn apply(_limits: Limits) -> Applied {
		Applied {
			priority_live: false,
			cpu_cap_live: false,
		}
	}
}
