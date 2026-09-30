//! The CLI's view of the transports in `yas_client::transport`: the same
//! connectors, with this executable as the server/proxy to auto-start and the
//! `YAS_PROXY` policy applied.

pub use yas_client::transport::*;

use yas_client::ConnectOptions;

fn current_exe() -> Result<std::path::PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("cannot locate yas executable: {error}"))
}

/// Connect options for a CLI invocation: named `yas-cli`, identified as
/// `YAS_CLIENT_IDENTIFIER` says, using `hub` for share links, the local proxy
/// unless `YAS_PROXY=0`, and auto-starting the local server from this
/// executable.
pub fn cli_options(hub: &str) -> ConnectOptions {
    let mut options = ConnectOptions::named("yas-cli");
    options.hello.client_release = env!("CARGO_PKG_VERSION").to_string();
    options.hello.identifier = std::env::var("YAS_CLIENT_IDENTIFIER").ok();
    options.hub = hub.to_string();
    options.proxy = proxy_enabled();
    options.executable = std::env::current_exe().ok();
    options.start_local = true;
    options
}

/// Make sure a local server answers on `socket_path`, starting this
/// executable's `yas server` detached when none does.
pub async fn ensure_local_server(socket_path: &str) -> Result<(), String> {
    yas_client::transport::ensure_local_server(socket_path, None, &current_exe()?).await
}

/// Make sure the local proxy runs, starting this executable's `yas proxy`
/// when it does not, and return its socket path.
pub async fn ensure_proxy() -> Result<String, String> {
    yas_client::transport::ensure_proxy(&current_exe()?).await
}
