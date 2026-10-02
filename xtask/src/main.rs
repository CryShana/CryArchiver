mod calibrate;
mod project_size;

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
	#[command(about = "Build a synthetic project in a temp folder and print how many bytes each table takes")]
	ProjectSize {
		#[arg(long, default_value_t = 100_000)]
		files: usize,
	},
}

fn main() -> ExitCode {
	let result = match Cli::parse().task {
		Task::Calibrate { corpus, out } => {
			if let Err(message) = calibrate::check_out(&out) {
				eprintln!("error: {message}");
				return ExitCode::from(2);
			}

			tokio::runtime::Runtime::new()
				.expect("tokio runtime starts")
				.block_on(calibrate::run(&corpus, &out))
		}
		Task::ProjectSize { files } => project_size::run(files),
	};

	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(message) => {
			eprintln!("error: {message}");
			ExitCode::FAILURE
		}
	}
}
