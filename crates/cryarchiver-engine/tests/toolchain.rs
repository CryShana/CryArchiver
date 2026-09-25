use cryarchiver_engine::toolchain::{ToolConfig, Toolchain};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn detects_the_installed_toolchain_and_caches_it() {
	let cache = tempfile::tempdir().unwrap();
	let config = ToolConfig {
		ffmpeg: None,
		cache_dir: Some(cache.path().to_path_buf()),
	};
	let cancel = CancellationToken::new();

	let tools = Toolchain::detect(&config, &cancel).await.unwrap();

	assert!(tools.encoders.contains("libopus"));
	assert!(tools.audio_problems().is_empty(), "{:?}", tools.audio_problems());
	assert!(cache.path().join("toolchain.json").is_file());
	assert_eq!(Toolchain::detect(&config, &cancel).await.unwrap(), tools);
}
