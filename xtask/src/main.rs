mod calibrate;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "Development tasks for CryArchiver")]
struct Cli {
	#[command(subcommand)]
	task: Task,
}

#[derive(Subcommand)]
enum Task {
	#[command(about = "Measure a read-only corpus and write the calibration tables")]
	Calibrate {
		#[arg(long, value_name = "DIR")]
		corpus: PathBuf,
		#[arg(long, value_name = "FILE")]
		out: PathBuf,
	},
}

fn main() -> ExitCode {
	let Task::Calibrate { corpus, out } = Cli::parse().task;
	if let Err(message) = calibrate::check_out(&out) {
		eprintln!("error: {message}");
		return ExitCode::from(2);
	}

	let runtime = tokio::runtime::Runtime::new().expect("tokio runtime starts");

	match runtime.block_on(calibrate::run(&corpus, &out)) {
		Ok(()) => ExitCode::SUCCESS,
		Err(message) => {
			eprintln!("error: {message}");
			ExitCode::FAILURE
		}
	}
}
