//! Connection and HELLO options.

use std::path::PathBuf;

use yas_wire::{Extension, Extensions, core::FamilyOffer, family};

use crate::error::{Error, Result};

/// What this client says in HELLO.
#[derive(Clone, Debug)]
pub struct HelloOptions {
    /// Shown in the server's client list (`yas client list`).
    pub client_name: String,
    /// This client's release, shown next to its name.
    pub client_release: String,
    /// Families to offer, by ID (`yas_wire::family::*`); `None` offers every
    /// family this build knows. Core is always implied.
    pub families: Option<Vec<u16>>,
    /// Families whose absence must fail HELLO instead of being left out.
    pub required: Vec<u16>,
    /// Ask for a read-only session: the server then selects only passive
    /// Terminal/Surface/Media/Font operations (watch, read, capture, journal,
    /// output, wait) and refuses every mutation. The marker is a required
    /// HELLO extension, so a server that does not understand it refuses the
    /// session instead of silently granting full control.
    pub read_only: bool,
    /// Any text to tell this client apart by in the server's client list
    /// (`yas client list`): a person, a device, the app embedding it. That
    /// list shows each client's Terminal and Surface views, so this is what
    /// says who a view's size came from. The server passes it on as is: being
    /// a `String`, it is UTF-8, and it must take at most
    /// [`MAX_CLIENT_IDENTIFIER_BYTES`](yas_wire::core::MAX_CLIENT_IDENTIFIER_BYTES)
    /// (1 KiB), the only things asked of it; several clients may report the
    /// same one. `None`
    /// reports none. [`Client::set_identifier`](crate::Client::set_identifier)
    /// replaces it later.
    pub identifier: Option<String>,
}

impl Default for HelloOptions {
    fn default() -> Self {
        Self {
            client_name: "yas-client".into(),
            client_release: env!("CARGO_PKG_VERSION").into(),
            families: None,
            required: Vec::new(),
            read_only: false,
            identifier: None,
        }
    }
}

impl HelloOptions {
    /// Default options with a client name.
    pub fn named(client_name: impl Into<String>) -> Self {
        Self {
            client_name: client_name.into(),
            ..Self::default()
        }
    }

    /// Offer only these families.
    pub fn families(mut self, families: impl IntoIterator<Item = u16>) -> Self {
        self.families = Some(families.into_iter().collect());
        self
    }

    /// Fail HELLO unless these families are selected.
    pub fn require(mut self, families: impl IntoIterator<Item = u16>) -> Self {
        self.required.extend(families);
        self
    }

    /// Ask for a read-only session.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Report `identifier` as this client's identifier.
    pub fn identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    pub(crate) fn family_offers(&self) -> Vec<FamilyOffer> {
        yas_wire::schema::FAMILIES
            .iter()
            .filter(|metadata| metadata.id != family::CORE)
            .filter(|metadata| {
                self.families
                    .as_ref()
                    .is_none_or(|families| families.contains(&metadata.id))
                    || self.required.contains(&metadata.id)
            })
            .map(|metadata| FamilyOffer {
                family_id: metadata.id,
                versions: vec![metadata.version],
                required: self.required.contains(&metadata.id),
            })
            .collect()
    }

    pub(crate) fn extensions(&self) -> Result<Extensions> {
        // Say what this build is, so a client list can name it: the
        // extension is optional and a peer that ignores it loses nothing.
        let mut extensions = vec![
            yas_wire::core::Platform::current()
                .extension(yas_wire::schema::core::CLIENT_HELLO_PLATFORM_EXTENSION as u16)
                .map_err(|error| Error::protocol(format!("invalid platform extension: {error}")))?,
        ];
        if self.read_only {
            extensions.push(read_only_extension());
        }
        if let Some(identifier) = &self.identifier {
            extensions.push(
                yas_wire::core::client_identifier_extension(identifier).map_err(|error| {
                    Error::invalid(format!("invalid client identifier: {error}"))
                })?,
            );
        }
        extensions.sort_by_key(|extension| extension.tag);
        Ok(Extensions(extensions))
    }
}

/// The ClientHello extension that makes a session read-only.
pub fn read_only_extension() -> Extension {
    Extension {
        tag: yas_wire::schema::core::CLIENT_HELLO_READ_ONLY_SESSION_EXTENSION as u16,
        required: true,
        value: Vec::new(),
    }
}

/// How to reach a target, plus the HELLO to send.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// HELLO contents.
    pub hello: HelloOptions,
    /// Signalling hub for `share:` targets without an explicit `?hub=`.
    pub hub: String,
    /// Route `ssh:`, `tcp:`, `ws(s)://`, `uplink:` and `share:` targets
    /// through the shared per-user `yas proxy-daemon` (connection reuse
    /// across processes). Needs [`ConnectOptions::executable`]. Off by
    /// default for libraries; the `yas` CLI turns it on unless `YAS_PROXY=0`.
    pub proxy: bool,
    /// The `yas` executable, used to auto-start local servers and the proxy
    /// daemon. `None` (the default) never starts anything.
    pub executable: Option<PathBuf>,
    /// Start `local` / `local:NAME` servers that are not running (needs
    /// [`ConnectOptions::executable`]).
    pub start_local: bool,
    /// Resolve bare target names through the home server's remotes catalogue
    /// (the `remotes` KV key on the local default server).
    pub remotes: bool,
    /// The SSH pool for `ssh:` targets. Build one with
    /// [`yas_ssh::SshPool::with_options`] to use in-memory keys and pinned
    /// host keys, and reuse it across connections to share SSH sessions.
    /// `None` uses a fresh default pool (ssh-agent, key files,
    /// `~/.ssh/known_hosts`, like `ssh`) for each connection.
    pub ssh: Option<yas_ssh::SshPool>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            hello: HelloOptions::default(),
            hub: yas_webrtc_forwarder::DEFAULT_HUB_URL.into(),
            proxy: false,
            executable: None,
            start_local: false,
            remotes: true,
            ssh: None,
        }
    }
}

impl ConnectOptions {
    /// Default options with a HELLO client name.
    pub fn named(client_name: impl Into<String>) -> Self {
        Self {
            hello: HelloOptions::named(client_name),
            ..Self::default()
        }
    }

    /// Use `executable` to start local servers on demand.
    pub fn start_local_with(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self.start_local = true;
        self
    }
}
