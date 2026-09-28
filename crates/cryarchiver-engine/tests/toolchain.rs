use cryarchiver_engine::toolchain::{MediaKind, ToolConfig, Toolchain};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn detects_the_installed_toolchain_every_time() {
	let config = ToolConfig { ffmpeg: None };
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
	assert!(tools.has_vmaf(), "{tools:?}");
	assert!(
		tools
			.svt_av1_release
			.as_deref()
			.is_some_and(|release| release.split('.').count() == 3),
		"{tools:?}"
	);
	assert!(tools.libvpx_release.is_some(), "{tools:?}");
	assert!(
		tools.build_id.starts_with(|c: char| c.is_ascii_alphanumeric()),
		"{tools:?}"
	);
	assert_eq!(Toolchain::detect(&config, &cancel).await.unwrap(), tools);
}
