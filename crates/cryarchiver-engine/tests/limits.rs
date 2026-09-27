use cryarchiver_engine::process::limits::{self, Limits};

const LIMITED: Limits = Limits {
	low_priority: true,
	cpu_cap: Some(25),
};

#[cfg(windows)]
mod windows_limits {
	use std::os::windows::io::AsRawHandle;
	use std::process::{Child, Command, Stdio};

	use windows::Win32::Foundation::HANDLE;
	use windows::Win32::System::Threading::{
		GetCurrentProcess, GetPriorityClass, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
	};

	use super::*;

	fn priority_class(process: HANDLE) -> u32 {
		// SAFETY: the handle is valid for the call.
		unsafe { GetPriorityClass(process) }
	}

	fn own_priority_class() -> u32 {
		// SAFETY: the pseudo handle of the current process is always valid.
		priority_class(unsafe { GetCurrentProcess() })
	}

	fn child_priority_class(child: &Child) -> u32 {
		priority_class(HANDLE(child.as_raw_handle()))
	}

	fn sleeping() -> Command {
		let mut command = Command::new("ping");
		command.args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null());
		command
	}

	fn sleeper() -> Child {
		sleeping().spawn().unwrap()
	}

	#[test]
	fn the_limit_reaches_running_processes_and_lifts_again() {
		limits::apply(Limits::default());
		let mut running = sleeper();
		let mut outside = limits::spawn_outside_limits(&mut sleeping()).unwrap();

		let applied = limits::apply(LIMITED);

		assert!(applied.priority_live && applied.cpu_cap_live, "{applied:?}");
		assert_eq!(own_priority_class(), IDLE_PRIORITY_CLASS.0);
		assert_eq!(child_priority_class(&running), IDLE_PRIORITY_CLASS.0);
		assert_eq!(
			child_priority_class(&outside),
			NORMAL_PRIORITY_CLASS.0,
			"a program that isn't one of our tools stays out of the limit"
		);
		assert_eq!(limits::current(), LIMITED);
		outside.kill().unwrap();
		outside.wait().unwrap();

		let lifted = limits::apply(Limits::default());

		assert!(lifted.priority_live && lifted.cpu_cap_live, "{lifted:?}");
		assert_eq!(own_priority_class(), NORMAL_PRIORITY_CLASS.0);
		assert_eq!(child_priority_class(&running), NORMAL_PRIORITY_CLASS.0);

		running.kill().unwrap();
		running.wait().unwrap();
	}
}

#[cfg(target_os = "linux")]
mod linux_limits {
	use std::path::Path;
	use std::time::Duration;

	use cryarchiver_engine::process::{StdoutMode, ToolCommand, run};
	use tokio_util::sync::CancellationToken;

	use super::*;

	#[tokio::test]
	async fn new_tools_start_at_low_priority() {
		let applied = limits::apply(LIMITED);
		let cmd = ToolCommand::new(Path::new("nice"), Duration::from_secs(10));

		let output = run(&cmd, StdoutMode::Capture, |_| {}, &CancellationToken::new())
			.await
			.unwrap();

		assert!(!applied.cpu_cap_live);
		assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "19");
	}
}
