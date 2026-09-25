mod inputs;

use clap::Parser;

#[derive(Parser)]
#[command(name = "cryarchiver", version = cryarchiver_engine::VERSION, about = "Makes media files smaller without thinking about encoders")]
struct Cli {}

fn main() {
	Cli::parse();
}
