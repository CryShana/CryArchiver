#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum Speed {
	Slow,
	#[default]
	Moderate,
	Fast,
}
