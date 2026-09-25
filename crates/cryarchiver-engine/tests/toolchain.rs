use cryarchiver_engine::toolchain::{MediaKind, ToolConfig, Toolchain};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn detects_the_installed_toolchain_and_caches_it() {
	let cache = tempfile::tempdir().unwrap();
	let config = ToolConfig {
		ffmpeg: None,
		cache_dir: Some(cache.path().to_path_buf()),
		refresh: false,
	};
	let cancel = CancellationToken::new();

	let tools = Toolchain::detect(&config, &cancel).await.unwrap();

	assert!(tools.encoders.contains("libopus"));
	assert!(
		tools.problems(MediaKind::Audio).is_empty(),
		"{:?}",
		tools.problems(MediaKind::Audio)
	);
	assert!(
		tools.problems(MediaKind::Video).is_empty(),
		"{:?}",
		tools.problems(MediaKind::Video)
	);
	assert!(tools.svt_settings && tools.colour_tags && tools.vp9_alpha, "{tools:?}");
	assert!(tools.has_av1_decoder());
	assert!(cache.path().join("toolchain.json").is_file());
	assert_eq!(Toolchain::detect(&config, &cancel).await.unwrap(), tools);
}
