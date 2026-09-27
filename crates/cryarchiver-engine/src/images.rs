pub mod avif;
pub mod decode;
pub mod jxl;
pub mod plan;
pub mod props;
pub mod structure;

use tokio_util::sync::CancellationToken;

use crate::outcome::{ErrorKind, JobError};

pub async fn blocking<T: Send + 'static>(
	cancel: &CancellationToken,
	work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, JobError> {
	let handle = tokio::task::spawn_blocking(work);
	tokio::select! {
		joined = handle => joined.map_err(|err| ErrorKind::EncodeFailed(format!("an image worker failed: {err}")).into()),
		() = cancel.cancelled() => Err(ErrorKind::Cancelled.into()),
	}
}
