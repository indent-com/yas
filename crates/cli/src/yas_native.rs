//! The CLI's native YAS client is `yas_client::native`: one implementation,
//! shared with every embedder. This module only adds how a CLI invocation
//! connects.

pub(crate) use yas_client::native::{
    MAX_COLLECTED_TRANSFER_BYTES, NativeClient, NativeFrameReader, NativeFrameSender,
};

/// Connect to `on` (or the configured default target, else the local server,
/// started on demand) the way every CLI command does.
pub(crate) async fn connect(on: Option<&str>, hub: &str) -> Result<NativeClient, String> {
    Ok(NativeClient::connect(on, &crate::transport::cli_options(hub)).await?)
}
