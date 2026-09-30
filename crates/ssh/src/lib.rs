//! Embedded SSH client for yas.
//!
//! Provides connection pooling, ssh-agent authentication, `~/.ssh/config`
//! parsing, and `direct-streamlocal` channel forwarding for connecting to
//! remote yas-servers without shelling out to the system `ssh` binary.
//!
//! By default a pool behaves like `ssh`: ssh-agent, then key files, with
//! host keys pinned in `~/.ssh/known_hosts` (trust on first use). A service
//! holding credentials in memory builds its pool with
//! [`SshPool::with_options`] instead: private keys (or a password) passed as
//! values, host keys pinned by the caller ([`HostKeyPolicy::Pinned`]) or
//! checked by a callback ([`HostKeyPolicy::Verify`]), and no file, agent or
//! `~/.ssh/config` access unless asked for.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use russh::client;
#[cfg(unix)]
use russh::keys::agent;
use russh::keys::{self, PrivateKeyWithHashAlg};

// ── Error ──────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ssh: {0}")]
    Russh(#[from] russh::Error),
    #[error("ssh key: {0}")]
    Keys(#[from] keys::Error),
    #[error("ssh: {0}")]
    Io(#[from] std::io::Error),
    #[error("ssh: {0}")]
    Other(String),
    /// The server's host key was refused: not pinned, rejected by the
    /// [`HostKeyPolicy::Verify`] callback, or different from the one in
    /// known_hosts. `fingerprint` (`SHA256:…`) is the key it presented.
    #[error("ssh: {message}")]
    HostKey {
        host: String,
        port: u16,
        fingerprint: String,
        message: String,
    },
}

// ── Caller-supplied credentials and host-key policy ────────────────────

/// What a [`HostKeyPolicy::Verify`] callback is asked about.
#[derive(Debug)]
pub struct HostKeyCheck<'a> {
    /// The host as connected to (after `~/.ssh/config` resolution, if used).
    pub host: &'a str,
    /// The TCP port.
    pub port: u16,
    /// The key the server presented.
    pub key: &'a keys::PublicKey,
}

impl HostKeyCheck<'_> {
    /// The presented key as an OpenSSH public-key line (`ssh-ed25519 AAAA…`).
    pub fn openssh(&self) -> String {
        public_key_openssh(self.key)
    }

    /// The presented key's `SHA256:…` fingerprint.
    pub fn fingerprint(&self) -> String {
        fingerprint(self.key)
    }
}

/// How a pool decides whether to trust a server's host key.
#[derive(Clone, Default)]
pub enum HostKeyPolicy {
    /// `~/.ssh/known_hosts` (or `YAS_SSH_KNOWN_HOSTS`): unknown hosts are
    /// learned and recorded, changed keys are refused.
    #[default]
    KnownHosts,
    /// Accept exactly these keys (any host). Nothing is read or written.
    Pinned(Vec<keys::PublicKey>),
    /// Ask the callback; `true` accepts. Use it to pin into your own store
    /// (trust on first use) or to compare fingerprints.
    Verify(Arc<dyn Fn(&HostKeyCheck<'_>) -> bool + Send + Sync>),
}

impl std::fmt::Debug for HostKeyPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KnownHosts => f.write_str("KnownHosts"),
            Self::Pinned(keys) => f
                .debug_tuple("Pinned")
                .field(&keys.iter().map(fingerprint).collect::<Vec<_>>())
                .finish(),
            Self::Verify(_) => f.write_str("Verify(..)"),
        }
    }
}

/// Credentials and policy for an [`SshPool`].
#[derive(Clone, Debug)]
pub struct SshOptions {
    /// Private keys tried in order before anything else.
    pub keys: Vec<Arc<keys::PrivateKey>>,
    /// A password tried after the keys.
    pub password: Option<String>,
    /// Try ssh-agent (`SSH_AUTH_SOCK`).
    pub agent: bool,
    /// Try `~/.ssh/id_*` and `IdentityFile`s from `~/.ssh/config`.
    pub key_files: bool,
    /// Read `~/.ssh/config` (Hostname, User, Port, IdentityFile).
    pub config_file: bool,
    /// Host-key trust.
    pub host_keys: HostKeyPolicy,
    /// Port when neither the URI nor `~/.ssh/config` names one (default 22).
    pub port: Option<u16>,
    /// Install YAS on the remote (`curl https://yas.run | sh` into
    /// `~/.local`) and start its server when no socket answers. Installing
    /// happens only when `yas` is not found there.
    pub install: bool,
    /// How to reach the remote server: its socket, a command, or both.
    pub mode: SshMode,
    /// The command [`SshMode::Exec`] runs on the remote, verbatim, through
    /// the user's login shell. Default: `yas connect --stdio` (plus
    /// `--on socket:PATH` when the URI names a socket).
    pub remote_command: Option<String>,
}

/// How [`SshPool::connect_yas`] reaches the remote YAS server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SshMode {
    /// Forward to the remote YAS socket (`direct-streamlocal`), starting its
    /// server (and installing YAS, with [`SshOptions::install`]) when none
    /// answers. When the SSH server refuses stream-local forwarding, or no
    /// socket path can be worked out because the host has no POSIX shell (as
    /// on Windows), run `yas connect --stdio` over an exec channel instead.
    #[default]
    Auto,
    /// Only forward to the socket.
    Socket,
    /// Only run [`SshOptions::remote_command`] and speak YAS over its stdin
    /// and stdout. Needs neither a POSIX shell nor stream-local forwarding:
    /// Windows hosts, locked-down `sshd`s. Nothing is installed or started
    /// beyond what that command does (`yas connect --stdio` starts the
    /// user's server when none runs).
    Exec,
}

impl Default for SshOptions {
    /// Behave like `ssh`: agent, key files, `~/.ssh/config`, known_hosts.
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            password: None,
            agent: true,
            key_files: true,
            config_file: true,
            host_keys: HostKeyPolicy::KnownHosts,
            port: None,
            install: true,
            mode: SshMode::Auto,
            remote_command: None,
        }
    }
}

impl SshOptions {
    /// Only what the caller passes: no agent, no key files, no
    /// `~/.ssh/config`, and host keys pinned to `host_keys`.
    pub fn in_memory(host_keys: HostKeyPolicy) -> Self {
        Self {
            agent: false,
            key_files: false,
            config_file: false,
            host_keys,
            ..Self::default()
        }
    }

    /// Add a private key in OpenSSH (or PEM) text form.
    pub fn with_private_key(mut self, text: &str, passphrase: Option<&str>) -> Result<Self, Error> {
        self.keys
            .push(Arc::new(keys::decode_secret_key(text, passphrase)?));
        Ok(self)
    }

    /// Set a password.
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }
}

/// Parse an OpenSSH public key: `ssh-ed25519 AAAA… [comment]`, a
/// `known_hosts`-style `host ssh-ed25519 AAAA…` line, or bare base64.
pub fn parse_public_key(text: &str) -> Result<keys::PublicKey, Error> {
    let text = text.trim();
    if let Ok(key) = keys::PublicKey::from_openssh(text) {
        return Ok(key);
    }
    let fields: Vec<&str> = text.split_whitespace().collect();
    // known_hosts line: hosts, algorithm, base64
    if fields.len() >= 3
        && let Ok(key) = keys::PublicKey::from_openssh(&fields[1..].join(" "))
    {
        return Ok(key);
    }
    let base64 = fields
        .iter()
        .find(|field| field.starts_with("AAAA"))
        .copied()
        .unwrap_or(text);
    Ok(keys::parse_public_key_base64(base64)?)
}

/// A key's `SHA256:…` fingerprint, as `ssh-keygen -l` prints it.
pub fn fingerprint(key: &keys::PublicKey) -> String {
    key.fingerprint(keys::HashAlg::Sha256).to_string()
}

/// A key as an OpenSSH public-key line (without comment).
pub fn public_key_openssh(key: &keys::PublicKey) -> String {
    key.to_openssh().unwrap_or_default()
}

// ── Shell scripts run on the remote ────────────────────────────────────

/// Common secure automatic-path resolver run on the remote host. The remote
/// SSH process is the Unix-socket client, so it must validate filesystem
/// ownership there; local `SO_PEERCRED` cannot cross an SSH channel.
const SOCKET_SEARCH_COMMON: &str = r#"N="${YAS_SERVER_NAME:-default}"; I="$(id -u 2>/dev/null || true)"; U="$(id -un 2>/dev/null || true)"; case "$N" in ""|*[!A-Za-z0-9._-]*|*.) N=default;; esac; [ "${#N}" -le 64 ] || N=default; case "$U" in ""|*[!A-Za-z0-9._-]*|*.) U=;; esac; [ "${#U}" -le 64 ] || U=; sm() { stat -c "%u %a" "$1" 2>/dev/null || stat -f "%u %Lp" "$1" 2>/dev/null; }; priv() { [ -d "$1" ] && [ ! -L "$1" ] || return 1; M="$(sm "$1")" || return 1; O="${M%% *}"; V="${M#* }"; [ "$O" = "$I" ] || return 1; case "$V" in 700|0700) return 0;; *) return 1;; esac; }; system_dir() { [ -d "$1" ] && [ ! -L "$1" ] || return 1; M="$(sm "$1")" || return 1; O="${M%% *}"; V="${M#* }"; [ "$O" = 0 ] || return 1; case "$V" in [0-7][0145][0145]|0[0-7][0145][0145]) return 0;; *) return 1;; esac; }; own_dir() { [ -d "$1" ] && [ ! -L "$1" ] || return 1; M="$(sm "$1")" || return 1; [ "${M%% *}" = "$I" ]; }; sticky() { [ -d "$1" ] && [ ! -L "$1" ] || return 1; M="$(sm "$1")" || return 1; O="${M%% *}"; V="${M#* }"; [ "$O" = 0 ] || return 1; case "$V" in 1777|01777) return 0;; *) return 1;; esac; }; sock() { [ -S "$1" ] && [ ! -L "$1" ] || return 1; M="$(sm "$1")" || return 1; O="${M%% *}"; V="${M#* }"; [ "$O" = "$I" ] || return 1; case "$V" in 600|0600|700|0700) return 0;; *) return 1;; esac; }; fits() { L="$(printf "%s" "$1" | wc -c | tr -d " ")"; [ "$L" -le 103 ]; }; emit() { fits "$1" || return 1; printf "%s\n" "$1"; exit 0; }; prep() { if [ ! -e "$1" ]; then (umask 077 && mkdir "$1") || return 1; fi; own_dir "$1" || return 1; chmod 700 "$1" || return 1; priv "$1"; }; "#;

const SOCKET_SEARCH_CANDIDATES: &str = r#"if [ -n "$XDG_RUNTIME_DIR" ] && priv "$XDG_RUNTIME_DIR"; then S="$XDG_RUNTIME_DIR/yas/$P-$N.sock"; priv "$XDG_RUNTIME_DIR/yas" && sock "$S" && emit "$S"; fi; if [ -n "$TMPDIR" ] && priv "$TMPDIR"; then S="$TMPDIR/yas/$P-$N.sock"; priv "$TMPDIR/yas" && sock "$S" && emit "$S"; elif [ -n "$TMPDIR" ] && sticky "$TMPDIR"; then S="$TMPDIR/yas-$I/$P-$N.sock"; priv "$TMPDIR/yas-$I" && sock "$S" && emit "$S"; fi; R="/run/user/$I"; if priv "$R"; then S="$R/yas/$P-$N.sock"; priv "$R/yas" && sock "$S" && emit "$S"; fi; if sticky /tmp; then S="/tmp/yas-$I/$P-$N.sock"; priv "/tmp/yas-$I" && sock "$S" && emit "$S"; fi; if [ -n "$XDG_RUNTIME_DIR" ] && priv "$XDG_RUNTIME_DIR" && prep "$XDG_RUNTIME_DIR/yas"; then emit "$XDG_RUNTIME_DIR/yas/$P-$N.sock"; fi; if [ -n "$TMPDIR" ] && priv "$TMPDIR" && prep "$TMPDIR/yas"; then emit "$TMPDIR/yas/$P-$N.sock"; fi; if [ -n "$TMPDIR" ] && sticky "$TMPDIR" && prep "$TMPDIR/yas-$I"; then emit "$TMPDIR/yas-$I/$P-$N.sock"; fi; if priv "$R" && prep "$R/yas"; then emit "$R/yas/$P-$N.sock"; fi; if sticky /tmp && prep "/tmp/yas-$I"; then emit "/tmp/yas-$I/$P-$N.sock"; fi; printf "\n"'"#;

/// Resolve one native remote automatic socket without trusting
/// attacker-controlled TMPDIR/XDG paths.
fn socket_search_script() -> String {
    let prefix = "P=yas; ";
    let explicit = r#"[ -n "$YAS_SOCK" ] && { printf "%s\n" "$YAS_SOCK"; exit 0; }; "#;
    let packaged = r#"if [ -n "$U" ] && system_dir "/run/yas"; then D="/run/yas/$U"; S="$D/yas-$N.sock"; priv "$D" && sock "$S" && emit "$S"; S="/run/yas/$U-$N.sock"; sock "$S" && emit "$S"; fi; "#;
    [
        "sh -c '",
        prefix,
        SOCKET_SEARCH_COMMON,
        explicit,
        packaged,
        SOCKET_SEARCH_CANDIDATES,
    ]
    .concat()
}

/// Escape a string for use inside double quotes in a POSIX shell.
/// Handles `\`, `$`, `` ` ``, and `"`.
fn dq_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' | '$' | '`' | '"' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Quote one complete POSIX shell word. The remote SSH server invokes the
/// user's login shell before our explicit `sh -c`, so the inner script needs a
/// second, independent quoting layer even when values inside it are already
/// safe for double quotes.
fn sq_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Install yas on the remote if missing, then start `yas server` and
/// detach it from the session.
///
/// Wrapped in `sh -c` so the POSIX script runs correctly even when the
/// remote user's login shell is fish or another non-POSIX shell.  The
/// socket path is double-quote-escaped to avoid single-quote nesting
/// issues inside the outer `sh -c '…'` wrapper.
fn install_and_start_script_for(socket_path: &str, explicit_socket: bool) -> String {
    let escaped = dq_escape(socket_path);
    let socket_override = if explicit_socket {
        String::from("YAS_SOCK=\"$S\" ")
    } else {
        String::new()
    };
    let script = format!(
        "export PATH=\"$HOME/.local/bin:$PATH\"; \
         if ! command -v yas >/dev/null 2>&1; then \
           if command -v curl >/dev/null 2>&1; then YAS_PREFIX=\"$HOME/.local\" curl -sf https://yas.run | sh >&2; \
           elif command -v wget >/dev/null 2>&1; then YAS_PREFIX=\"$HOME/.local\" wget -qO- https://yas.run | sh >&2; fi; \
         fi; \
         S=\"{escaped}\"; \
         if [ -S \"$S\" ]; then \
           if command -v nc >/dev/null 2>&1; then nc -z -U \"$S\" 2>/dev/null || rm -f \"$S\"; \
           elif command -v socat >/dev/null 2>&1; then socat /dev/null \"UNIX-CONNECT:$S\" 2>/dev/null || rm -f \"$S\"; fi; \
         fi; \
         if ! [ -S \"$S\" ]; then \
           if command -v yas >/dev/null 2>&1; then {socket_override}nohup yas server </dev/null >/dev/null 2>&1 & \
           fi; \
         fi; \
         echo ok"
    );
    format!("sh -c {}", sq_escape(&script))
}

// ── SSH config resolution ──────────────────────────────────────────────

/// Resolved SSH settings for a host, from `~/.ssh/config`.
#[derive(Default)]
struct ResolvedConfig {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_files: Vec<PathBuf>,
    proxy_jump: Option<String>,
}

/// Minimal `~/.ssh/config` parser. Supports Host (with `*`/`?` globs),
/// Hostname, User, Port, IdentityFile, and ProxyJump.
fn resolve_ssh_config(host: &str) -> ResolvedConfig {
    let path = match home_dir() {
        Some(h) => h.join(".ssh").join("config"),
        None => return ResolvedConfig::default(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return ResolvedConfig::default(),
    };

    let mut result = ResolvedConfig::default();
    let mut in_matching_block = false;
    let mut in_global = true; // before the first Host line

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_ascii_whitespace() || c == '=') {
            Some((k, v)) => (k.trim(), v.trim().trim_start_matches('=')),
            None => continue,
        };
        let value = value.trim();
        if key.eq_ignore_ascii_case("Host") {
            in_global = false;
            in_matching_block = value
                .split_whitespace()
                .any(|pattern| host_matches(pattern, host));
            continue;
        }
        if !in_matching_block && !in_global {
            continue;
        }
        if key.eq_ignore_ascii_case("Hostname") && result.hostname.is_none() {
            result.hostname = Some(value.to_string());
        } else if key.eq_ignore_ascii_case("User") && result.user.is_none() {
            result.user = Some(value.to_string());
        } else if key.eq_ignore_ascii_case("Port") && result.port.is_none() {
            result.port = value.parse().ok();
        } else if key.eq_ignore_ascii_case("IdentityFile") {
            let expanded = expand_tilde(value);
            result.identity_files.push(PathBuf::from(expanded));
        } else if key.eq_ignore_ascii_case("ProxyJump") && result.proxy_jump.is_none() {
            result.proxy_jump = Some(value.to_string());
        }
    }
    result
}

/// Simple glob match supporting `*` (any chars) and `?` (one char).
fn host_matches(pattern: &str, host: &str) -> bool {
    let mut p = pattern.chars().peekable();
    let mut h = host.chars().peekable();
    host_matches_inner(&mut p, &mut h)
}

fn host_matches_inner(
    p: &mut std::iter::Peekable<std::str::Chars>,
    h: &mut std::iter::Peekable<std::str::Chars>,
) -> bool {
    while let Some(&pc) = p.peek() {
        match pc {
            '*' => {
                p.next();
                if p.peek().is_none() {
                    return true; // trailing * matches everything
                }
                // Try matching * against 0..N chars of h
                loop {
                    let mut p2 = p.clone();
                    let mut h2 = h.clone();
                    if host_matches_inner(&mut p2, &mut h2) {
                        return true;
                    }
                    if h.next().is_none() {
                        return false;
                    }
                }
            }
            '?' => {
                p.next();
                if h.next().is_none() {
                    return false;
                }
            }
            _ => {
                p.next();
                match h.next() {
                    Some(hc) if hc == pc => {}
                    _ => return false,
                }
            }
        }
    }
    h.peek().is_none()
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return format!("{}/{rest}", home.display());
    }
    path.to_string()
}

// ── Handler ────────────────────────────────────────────────────────────

struct SshHandler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
}

impl client::Handler for SshHandler {
    type Error = Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let keys::PublicKeyOrCertificate::PublicKey {
            key: server_public_key,
            ..
        } = server_public_key
        else {
            // YAS pins raw host keys; it does not maintain trusted SSH CAs.
            return Err(Error::Other(
                "SSH host certificates are not supported".into(),
            ));
        };
        match &self.policy {
            HostKeyPolicy::KnownHosts => {}
            HostKeyPolicy::Pinned(pinned) => {
                // Comments are labels, not identity.
                if pinned
                    .iter()
                    .any(|key| key.key_data() == server_public_key.key_data())
                {
                    return Ok(true);
                }
                return Err(self.refused(
                    server_public_key,
                    format!("is not one of the {} pinned key(s)", pinned.len()),
                ));
            }
            HostKeyPolicy::Verify(verify) => {
                let check = HostKeyCheck {
                    host: &self.host,
                    port: self.port,
                    key: server_public_key,
                };
                if verify(&check) {
                    return Ok(true);
                }
                return Err(self.refused(server_public_key, "was rejected".to_string()));
            }
        }
        let path = known_hosts_path().ok_or_else(|| {
            Error::Other(
                "cannot verify host keys: no home directory for ~/.ssh/known_hosts. \
                 Set HOME, or point YAS_SSH_KNOWN_HOSTS at a file."
                    .into(),
            )
        })?;

        // russh reports an unreadable known_hosts exactly like an absent one —
        // an empty key list — so a permissions problem would read as "new
        // host" and re-pin against whatever answered. Probe it here, where
        // absent and unreadable are still distinguishable.
        match std::fs::File::open(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::Other(format!(
                    "cannot read {}: {e}. Refusing rather than trusting an \
                     unverified host key.",
                    path.display()
                )));
            }
        }

        // Ask what is pinned for this host rather than "does the presented key
        // match", because the two differ in a way that matters. russh answers
        // `Ok(false)` both for a host it has never seen and for a host pinned
        // under a *different key algorithm* — so appending on `Ok(false)`, as
        // this did, let anyone bypass an ed25519 pin by presenting an RSA key.
        // Nothing here constrains the algorithm the server may offer.
        let recorded = keys::known_hosts::known_host_keys_path(&self.host, self.port, &path)
            .map_err(|e| {
                // A parse or IO failure means the pin could not be read, not
                // that there is none. This arm used to append and accept, so
                // one corrupt line — or a single non-UTF-8 byte anywhere in
                // the file, which fails the whole read — silently unpinned the
                // host and recorded whatever answered.
                Error::Other(format!(
                    "cannot parse {} for {}:{}: {e}. Refusing rather than \
                     trusting an unverified host key.",
                    path.display(),
                    self.host,
                    self.port
                ))
            })?;

        if recorded.is_empty() {
            // Genuinely unknown host: trust on first use, and record it so the
            // second use is checked. Write errors are fatal — a read-only
            // known_hosts silently re-learning on every connection is
            // indistinguishable from having no pin at all.
            keys::known_hosts::learn_known_hosts_path(
                &self.host,
                self.port,
                server_public_key,
                &path,
            )
            .map_err(|e| {
                Error::Other(format!(
                    "cannot record host key for {}:{} in {}: {e}",
                    self.host,
                    self.port,
                    path.display()
                ))
            })?;
            return Ok(true);
        }

        if recorded.iter().any(|(_, k)| k == server_public_key) {
            return Ok(true);
        }

        Err(self.refused(
            server_public_key,
            format!(
                "does not match the {} key(s) recorded in {}! This could \
                 indicate a man-in-the-middle attack. Remove the old entry to \
                 continue.",
                recorded.len(),
                path.display()
            ),
        ))
    }
}

impl SshHandler {
    fn refused(&self, key: &keys::PublicKey, why: String) -> Error {
        let fingerprint = fingerprint(key);
        Error::HostKey {
            message: format!(
                "host key {fingerprint} for {}:{} {why}",
                self.host, self.port
            ),
            host: self.host.clone(),
            port: self.port,
            fingerprint,
        }
    }
}

/// Where host keys are pinned.
///
/// `YAS_SSH_KNOWN_HOSTS` overrides the location, which is also the escape
/// hatch for contexts with no `HOME` — a daemon or container — where the old
/// code accepted every host key unconditionally instead.
fn known_hosts_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("YAS_SSH_KNOWN_HOSTS")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    Some(home_dir()?.join(".ssh").join("known_hosts"))
}

// ── SSH Pool ───────────────────────────────────────────────────────────

/// SSH connection pool. Maintains persistent SSH connections and opens
/// channels on demand. Multiple channels share a single TCP+SSH connection
/// per host. Thread-safe and cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct SshPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    /// Cached connections keyed by `"user@host:port"`.
    connections: Mutex<HashMap<String, CachedConnection>>,
    options: SshOptions,
}

impl std::fmt::Debug for SshPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshPool")
            .field("host_keys", &self.inner.options.host_keys)
            .field("keys", &self.inner.options.keys.len())
            .finish_non_exhaustive()
    }
}

struct CachedConnection {
    handle: client::Handle<SshHandler>,
    /// Resolved native YAS socket path (cached after first resolution).
    remote_socket: Option<String>,
    /// How sessions reach this host once only exec works (the command is
    /// built per call: each may name its own socket).
    exec: Option<ExecRoute>,
    /// `yas --version` ran there (checked once per connection).
    plain_yas: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecRoute {
    /// `sh` runs: `yas connect --stdio` with ~/.local/bin and `YAS_SOCK`.
    Posix,
    /// No POSIX shell: a plain `yas connect --stdio`.
    Plain,
}

impl Default for SshPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SshPool {
    pub fn new() -> Self {
        Self::with_options(SshOptions::default())
    }

    /// A pool whose connections all use `options` (credentials, host-key
    /// policy). Pools share nothing; use one per credential set.
    pub fn with_options(options: SshOptions) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                connections: Mutex::new(HashMap::new()),
                options,
            }),
        }
    }

    /// The options this pool connects with.
    pub fn options(&self) -> &SshOptions {
        &self.inner.options
    }

    /// Open a `direct-streamlocal` channel to a remote yas-server.
    ///
    /// - Resolves `~/.ssh/config` for the target host.
    /// - Reuses an existing SSH connection if available.
    /// - Authenticates via ssh-agent, then falls back to key files.
    /// - If `remote_socket` is `None`, discovers the socket path on the remote.
    /// - Auto-starts yas-server on the remote if needed.
    /// - Returns a bidirectional `DuplexStream` connected to the remote socket.
    pub async fn connect(
        &self,
        host: &str,
        user: Option<&str>,
        remote_socket: Option<&str>,
    ) -> Result<tokio::io::DuplexStream, Error> {
        self.connect_yas(host, user, remote_socket).await
    }

    pub async fn connect_yas(
        &self,
        host: &str,
        user: Option<&str>,
        remote_socket: Option<&str>,
    ) -> Result<tokio::io::DuplexStream, Error> {
        self.connect_mode(host, user, remote_socket).await
    }

    async fn connect_mode(
        &self,
        host: &str,
        user: Option<&str>,
        remote_socket: Option<&str>,
    ) -> Result<tokio::io::DuplexStream, Error> {
        let socket_is_explicit = remote_socket.is_some();
        let options = &self.inner.options;
        let config = if options.config_file {
            resolve_ssh_config(host)
        } else {
            ResolvedConfig::default()
        };
        // An explicit "host:port" is not an SSH URI form (":" introduces the
        // socket path there), so ports come from options or ~/.ssh/config.
        let effective_host = config.hostname.as_deref().unwrap_or(host);
        let effective_user = user
            .map(String::from)
            .or(config.user.clone())
            .unwrap_or_else(current_username);
        let effective_port = config.port.or(options.port).unwrap_or(22);

        let key = format!("{effective_user}@{effective_host}:{effective_port}");

        // Phase 1: check if we need a new SSH connection.
        // Drop the lock before doing any network I/O so that connections to
        // *other* hosts can proceed concurrently.
        let mut conns = self.inner.connections.lock().await;
        let need_new = match conns.get(&key) {
            Some(cached) => cached.handle.is_closed(),
            None => true,
        };

        if need_new {
            // Release the lock while establishing the TCP + SSH connection —
            // this can take seconds (DNS, handshake, auth).
            drop(conns);
            let handle = establish_connection(
                effective_host,
                effective_port,
                &effective_user,
                &config,
                options,
            )
            .await?;
            conns = self.inner.connections.lock().await;
            // Another task may have raced us for the same key — prefer the
            // existing live connection to avoid duplicates.
            let still_need = match conns.get(&key) {
                Some(cached) => cached.handle.is_closed(),
                None => true,
            };
            if still_need {
                conns.insert(
                    key.clone(),
                    CachedConnection {
                        handle,
                        remote_socket: None,
                        exec: None,
                        plain_yas: false,
                    },
                );
            }
        }

        let cached = conns.get_mut(&key).unwrap();
        let host_label = effective_host;

        if options.mode == SshMode::Exec {
            let command = match &options.remote_command {
                Some(command) if remote_socket.is_some() => {
                    return Err(Error::Other(format!(
                        "an SSH URI socket path cannot be combined with the custom remote command `{command}`"
                    )));
                }
                Some(command) => command.clone(),
                None => plain_remote_command(remote_socket)?,
            };
            if options.remote_command.is_none() && !cached.plain_yas {
                require_plain_yas(&cached.handle, host_label).await?;
                cached.plain_yas = true;
            }
            return open_exec(&cached.handle, &command).await;
        }
        if options.mode == SshMode::Auto
            && let Some(route) = cached.exec
        {
            // An earlier connection learned that only a command gets through.
            let command = match route {
                ExecRoute::Posix => posix_remote_command(remote_socket, options.install),
                ExecRoute::Plain => plain_remote_command(remote_socket)?,
            };
            return open_exec(&cached.handle, &command).await;
        }

        // Resolve remote socket path if not cached and not explicitly provided.
        let socket_path = if let Some(explicit) = remote_socket {
            explicit.to_string()
        } else if let Some(cached_path) = cached.remote_socket.as_ref() {
            cached_path.clone()
        } else {
            let found = exec_command(&cached.handle, &socket_search_script()).await;
            let path = found
                .as_ref()
                .map(|path| path.trim().to_string())
                .unwrap_or_default();
            if path.is_empty() {
                if options.mode == SshMode::Auto {
                    // No POSIX shell ran the search (a Windows host, say):
                    // a plain `yas connect --stdio` is the way in.
                    let command = plain_remote_command(None)?;
                    require_plain_yas(&cached.handle, host_label).await?;
                    cached.plain_yas = true;
                    let stream = open_exec(&cached.handle, &command).await?;
                    cached.exec = Some(ExecRoute::Plain);
                    return Ok(stream);
                }
                return Err(found.err().unwrap_or_else(|| {
                    Error::Other("could not determine remote YAS socket path".to_string())
                }));
            }
            cached.remote_socket = Some(path.clone());
            path
        };

        let auto = options.mode == SshMode::Auto;
        let mut last_err = match cached
            .handle
            .channel_open_direct_streamlocal(&socket_path)
            .await
        {
            Ok(channel) => return Ok(bridge_channel(channel)),
            Err(error) => error,
        };
        if !forwarding_refused(&last_err) {
            if !options.install {
                return Err(Error::Other(format!(
                    "cannot reach the YAS socket {socket_path} on {host_label}: {last_err}"
                )));
            }
            // Nobody answers on the socket: install yas if it is missing and
            // start its server, then retry with back-off while the server
            // creates the socket.
            let _ = exec_command(
                &cached.handle,
                &install_and_start_script_for(&socket_path, socket_is_explicit),
            )
            .await;
            for attempt in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
                match cached
                    .handle
                    .channel_open_direct_streamlocal(&socket_path)
                    .await
                {
                    Ok(channel) => return Ok(bridge_channel(channel)),
                    Err(error) => last_err = error,
                }
                if forwarding_refused(&last_err) {
                    break;
                }
            }
            if !forwarding_refused(&last_err) {
                return Err(Error::Other(format!(
                    "failed to connect to {socket_path} after install: {last_err}"
                )));
            }
        }
        if !auto {
            return Err(Error::Other(format!(
                "{host_label} does not forward to the YAS socket {socket_path} ({last_err}); \
                 SshMode::Auto or SshMode::Exec would run `yas connect --stdio` there instead"
            )));
        }
        // The SSH server does not forward to sockets at all (OpenSSH's
        // AllowStreamLocalForwarding no, or no support for it): have a remote
        // yas relay the session over an exec channel. Install it first only
        // if it is missing and installing is allowed.
        if !posix_yas_present(&cached.handle).await {
            if !options.install {
                return Err(Error::Other(format!(
                    "{host_label} does not forward to sockets and has no yas to run \
                     (installing is off)"
                )));
            }
            let _ = exec_command(
                &cached.handle,
                &install_and_start_script_for(&socket_path, socket_is_explicit),
            )
            .await;
            if !posix_yas_present(&cached.handle).await {
                return Err(Error::Other(format!(
                    "{host_label} does not forward to sockets, and installing yas there failed"
                )));
            }
        }
        let command = posix_remote_command(remote_socket, options.install);
        let stream = open_exec(&cached.handle, &command).await?;
        cached.exec = Some(ExecRoute::Posix);
        Ok(stream)
    }
}

/// Whether a failed `direct-streamlocal` open means the SSH server will not
/// forward to Unix sockets at all, rather than that nobody listens there.
fn forwarding_refused(error: &russh::Error) -> bool {
    matches!(
        error,
        russh::Error::ChannelOpenFailure(
            russh::ChannelOpenFailure::AdministrativelyProhibited
                | russh::ChannelOpenFailure::UnknownChannelType
        )
    )
}

/// `yas connect --stdio`, naming the URI's socket when it has one. The
/// command goes through whatever shell the remote user has (cmd.exe too),
/// so a socket path must not need quoting.
fn plain_remote_command(socket: Option<&str>) -> Result<String, Error> {
    let Some(socket) = socket else {
        return Ok("yas connect --stdio".to_string());
    };
    if socket.is_empty()
        || !socket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/\\._-:~+@%,".contains(c))
    {
        return Err(Error::Other(format!(
            "the socket path {socket:?} cannot be passed to a remote command unquoted"
        )));
    }
    Ok(format!("yas --on socket:{socket} connect --stdio"))
}

/// `yas connect --stdio` through `sh`, finding a yas installed in
/// `~/.local/bin` and keeping an explicit socket exact, as the start script
/// does. Without `start`, it only connects to a running server.
fn posix_remote_command(socket: Option<&str>, start: bool) -> String {
    let socket_override = socket
        .map(|path| format!("YAS_SOCK=\"{}\" ", dq_escape(path)))
        .unwrap_or_default();
    let no_start = if start { "" } else { " --no-start" };
    let script = format!(
        "PATH=\"$HOME/.local/bin:$PATH\"; export PATH; \
         {socket_override}exec yas connect --stdio{no_start}"
    );
    format!("sh -c {}", sq_escape(&script))
}

/// Whether `yas` runs on a POSIX remote (including `~/.local/bin`).
async fn posix_yas_present(handle: &client::Handle<SshHandler>) -> bool {
    let probe = sh_probe();
    exec_command(handle, &probe)
        .await
        .is_ok_and(|output| output.trim() == "present")
}

fn sh_probe() -> String {
    format!(
        "sh -c {}",
        sq_escape(
            "PATH=\"$HOME/.local/bin:$PATH\"; command -v yas >/dev/null 2>&1 && echo present"
        )
    )
}

/// Fail clearly when a plain `yas` does not run on the remote: without a
/// POSIX shell nothing can be installed, and a missing command would
/// otherwise only show as a session that closes during HELLO.
async fn require_plain_yas(handle: &client::Handle<SshHandler>, host: &str) -> Result<(), Error> {
    let version = exec_command(handle, "yas --version")
        .await
        .unwrap_or_default();
    if version.trim_start().starts_with("yas") {
        return Ok(());
    }
    Err(Error::Other(format!(
        "`yas --version` did not run on {host}: install YAS there and put it on the PATH \
         of non-interactive SSH commands"
    )))
}

/// Run `command` on an exec channel and bridge its stdin/stdout to a
/// `DuplexStream`. Half-closes travel both ways: when the caller stops
/// writing, the command sees EOF on stdin and may still answer.
async fn open_exec(
    handle: &client::Handle<SshHandler>,
    command: &str,
) -> Result<tokio::io::DuplexStream, Error> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command.as_bytes()).await?;
    let mut early = Vec::new();
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Success) => break,
            Some(russh::ChannelMsg::Failure) => {
                return Err(Error::Other(format!(
                    "the SSH server refused to run `{command}`"
                )));
            }
            Some(russh::ChannelMsg::Data { data }) => early.extend_from_slice(&data),
            Some(russh::ChannelMsg::Eof | russh::ChannelMsg::Close) | None => {
                return Err(Error::Other(format!(
                    "the SSH channel closed before `{command}` started"
                )));
            }
            Some(_) => {}
        }
    }
    let stream = channel.into_stream();
    let (client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut sr, mut sw) = tokio::io::split(server);
        let (mut cr, mut cw) = tokio::io::split(stream);
        let up = async {
            let _ = tokio::io::copy(&mut sr, &mut cw).await;
            let _ = cw.shutdown().await;
        };
        let down = async {
            if sw.write_all(&early).await.is_ok() {
                let _ = tokio::io::copy(&mut cr, &mut sw).await;
            }
            let _ = sw.shutdown().await;
        };
        tokio::join!(up, down);
    });
    Ok(client)
}

/// Bridge an SSH channel to a `DuplexStream` so callers get a standard
/// tokio type with no russh types leaking.
fn bridge_channel(channel: russh::Channel<russh::client::Msg>) -> tokio::io::DuplexStream {
    let stream = channel.into_stream();
    let (client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let (mut sr, mut sw) = tokio::io::split(server);
        let (mut cr, mut cw) = tokio::io::split(stream);
        tokio::select! {
            _ = tokio::io::copy(&mut cr, &mut sw) => {}
            _ = tokio::io::copy(&mut sr, &mut cw) => {}
        }
    });
    client
}

// ── Connection + Authentication ────────────────────────────────────────

async fn establish_connection(
    host: &str,
    port: u16,
    user: &str,
    config: &ResolvedConfig,
    options: &SshOptions,
) -> Result<client::Handle<SshHandler>, Error> {
    let ssh_config = client::Config {
        // Detect dead connections behind NATs/firewalls instead of hanging
        // indefinitely.  The SSH transport will send a keepalive packet
        // every 15 s and give up after 3 consecutive misses (~45 s).
        keepalive_interval: Some(std::time::Duration::from_secs(15)),
        keepalive_max: 3,
        // YAS requests are small frames, often several in a row: with Nagle's algorithm, one
        // waits for the ACK of the one before, which the server delays (40 ms on Linux) since it
        // has nothing to answer until the whole request is in.
        nodelay: true,
        ..Default::default()
    };

    let handler = SshHandler {
        host: host.to_string(),
        port,
        policy: options.host_keys.clone(),
    };

    let mut handle = client::connect(Arc::new(ssh_config), (host, port), handler).await?;
    let mut tried = Vec::new();

    // Caller-supplied keys first.
    if !options.keys.is_empty() {
        tried.push("supplied keys");
        for key in &options.keys {
            let hash_alg = handle.best_supported_rsa_hash().await.ok().flatten();
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::clone(key), hash_alg.flatten());
            match handle.authenticate_publickey(user, key_with_hash).await {
                Ok(russh::client::AuthResult::Success) => return Ok(handle),
                Ok(_) => continue,
                Err(e) => log::debug!("supplied key auth failed: {e}"),
            }
        }
    }

    if let Some(password) = &options.password {
        tried.push("password");
        if let Ok(russh::client::AuthResult::Success) =
            handle.authenticate_password(user, password.clone()).await
        {
            return Ok(handle);
        }
    }

    if options.agent {
        tried.push("ssh-agent");
        if try_agent_auth(&mut handle, user).await {
            return Ok(handle);
        }
    }

    if options.key_files {
        tried.push("key files");
        if try_key_file_auth(&mut handle, user, config).await? {
            return Ok(handle);
        }
    }

    Err(Error::Other(format!(
        "authentication failed for {user}@{host}:{port} (tried {})",
        if tried.is_empty() {
            "nothing: no credentials configured".to_string()
        } else {
            tried.join(", ")
        }
    )))
}

/// Try authenticating via ssh-agent. Returns true on success.
#[cfg(unix)]
async fn try_agent_auth(handle: &mut client::Handle<SshHandler>, user: &str) -> bool {
    let agent_path = match std::env::var("SSH_AUTH_SOCK") {
        Ok(p) if !p.is_empty() => p,
        _ => return false,
    };
    let stream = match tokio::net::UnixStream::connect(&agent_path).await {
        Ok(s) => s,
        Err(e) => {
            log::debug!("ssh-agent connect failed: {e}");
            return false;
        }
    };
    let mut agent = agent::client::AgentClient::connect(stream);
    let identities = match agent.request_identities().await {
        Ok(ids) => ids,
        Err(e) => {
            log::debug!("ssh-agent request_identities failed: {e}");
            return false;
        }
    };
    for identity in &identities {
        let public_key = identity.public_key().into_owned();
        match handle
            .authenticate_publickey_with(user, public_key, None, &mut agent)
            .await
        {
            Ok(russh::client::AuthResult::Success) => return true,
            Ok(_) => continue,
            Err(e) => {
                log::debug!("ssh-agent auth attempt failed: {e}");
                continue;
            }
        }
    }
    false
}

/// On non-Unix platforms, agent auth is not yet supported — fall back to key files.
#[cfg(not(unix))]
async fn try_agent_auth(_handle: &mut client::Handle<SshHandler>, _user: &str) -> bool {
    false
}

/// Try authenticating with key files. Returns true on success.
async fn try_key_file_auth(
    handle: &mut client::Handle<SshHandler>,
    user: &str,
    config: &ResolvedConfig,
) -> Result<bool, Error> {
    let home = match home_dir() {
        Some(h) => h,
        None => return Ok(false),
    };

    // Collect candidate key paths: explicit from config + defaults.
    let mut candidates: Vec<PathBuf> = config.identity_files.clone();
    for default in &["id_ed25519", "id_ecdsa", "id_rsa"] {
        let p = home.join(".ssh").join(default);
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }

    for path in &candidates {
        if !path.exists() {
            continue;
        }
        let key = match keys::load_secret_key(path, None) {
            Ok(k) => k,
            Err(e) => {
                log::debug!("could not load {}: {e}", path.display());
                continue;
            }
        };

        // Determine the best RSA hash algorithm if applicable.
        let hash_alg = handle.best_supported_rsa_hash().await.ok().flatten();
        let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg.flatten());

        match handle.authenticate_publickey(user, key_with_hash).await {
            Ok(russh::client::AuthResult::Success) => return Ok(true),
            Ok(_) => continue,
            Err(e) => {
                log::debug!("key auth failed for {}: {e}", path.display());
                continue;
            }
        }
    }
    Ok(false)
}

// ── Remote command execution ───────────────────────────────────────────

/// Execute a command on the remote and return its stdout.
async fn exec_command(handle: &client::Handle<SshHandler>, cmd: &str) -> Result<String, Error> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd.as_bytes()).await?;

    let mut output = Vec::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => output.extend_from_slice(&data),
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
            // The server would not run it: no Close need follow.
            russh::ChannelMsg::Failure => {
                return Err(Error::Other(
                    "the SSH server refused an exec request".into(),
                ));
            }
            _ => continue,
        }
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

// ── Helpers ────────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
}

fn current_username() -> String {
    #[cfg(unix)]
    {
        std::env::var("USER").unwrap_or_else(|_| "root".into())
    }
    #[cfg(windows)]
    {
        std::env::var("USERNAME").unwrap_or_else(|_| "user".into())
    }
}

/// Parse an SSH URI: `[user@]host[:/socket]`.
/// Returns `(user, host, socket)`.
pub fn parse_ssh_uri(s: &str) -> (Option<String>, String, Option<String>) {
    let colon_start = s.find('@').map(|a| a + 1).unwrap_or(0);
    let (host_part, socket) = if let Some(rel) = s[colon_start..].find(':') {
        let pos = colon_start + rel;
        let path = &s[pos + 1..];
        if path.is_empty() {
            (s, None)
        } else {
            (&s[..pos], Some(path.to_string()))
        }
    } else {
        (s, None)
    };
    let (user, host) = if let Some(at) = host_part.rfind('@') {
        (
            Some(host_part[..at].to_string()),
            host_part[at + 1..].to_string(),
        )
    } else {
        (None, host_part.to_string())
    };
    (user, host, socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::client::Handler;
    use std::path::Path;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAiVpqLWTIigzpaNk7fXH5+QRGxbbMLM6XJ28iya08po";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJtJlwVPL88rnGVkDna6i1QqC5RVs5+X6cV+/x7MS4XA";
    /// A different *algorithm* for the same host — the bypass that used to
    /// read as "unknown host" rather than as a changed key.
    const KEY_RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDr4Cp7pJoEPmyqfRrTwJxojfO/6tEDd09BnDQXjz0tMw83/PXgMjig1EXfKn+zGfulY/GuktQb1FU3zg5f6MouxWfRDJnrX3RlsfaVEueK+ddtocOaVFgBB37kyRCcT5huNjJf6ixc+dnmaYZ5BRl0QbKQSfj9TeyaQxttxv81pTRN5uN6oOvTdbBR5p4+Px+kpuAVsdm9k5bNmlnm1N4MH1ueA1P4Rt/5YnHj4N47G6wfW/jNGz2tzt39zL/pezvxQl2ftI9gRHqFk7D8SD1mTB9fCewOaP8VmVCQio7hCMZf3+hmjGmLhtqHbXZDBKmHYe4BuIOpMT/ZD4P9dJop";

    #[test]
    fn remote_search_covers_private_native_defaults() {
        let native = socket_search_script();
        assert!(native.contains("$XDG_RUNTIME_DIR/yas/$P-$N.sock"));
        assert!(native.contains("$TMPDIR/yas-$I/$P-$N.sock"));
        assert!(native.contains("$R/yas/$P-$N.sock"));
        assert!(native.contains("/tmp/yas-$I/$P-$N.sock"));
        assert!(native.contains("priv"));
        assert!(native.contains("sock"));
        assert!(
            std::process::Command::new("sh")
                .args(["-n", "-c", &native])
                .status()
                .unwrap()
                .success()
        );
        assert!(native.contains("$YAS_SOCK"));
        assert!(native.contains("D=\"/run/yas/$U\""));
        assert!(native.contains("S=\"$D/yas-$N.sock\""));
        assert!(native.contains("/run/yas/$U-$N.sock"));
        assert!(native.contains("system_dir \"/run/yas\""));
        assert!(!native.contains("$XDG_RUNTIME_DIR/$P-$N.sock"));
        assert!(!native.contains("$TMPDIR/$P-$N.sock"));
        assert!(!native.contains("$R/$P-$N.sock"));
        assert!(!native.contains("/tmp/$P-$U-$N.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_search_prepares_private_xdg_child_and_returns_new_layout() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let runtime =
            std::env::temp_dir().join(format!("yas-ssh-resolver-{}-{unique}", std::process::id()));
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let name = format!("fixture-{}", std::process::id());
        let output = std::process::Command::new("sh")
            .args(["-c", &socket_search_script()])
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("YAS_SERVER_NAME", &name)
            .env_remove("TMPDIR")
            .env_remove("YAS_SOCK")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let resolved = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            resolved.trim(),
            runtime
                .join(format!("yas/yas-{name}.sock"))
                .to_str()
                .unwrap()
        );
        assert_eq!(
            std::fs::symlink_metadata(runtime.join("yas"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn automatic_start_does_not_freeze_prediction_into_explicit_override() {
        let predicted = "/tmp/yas-1000/yas-default.sock";
        let automatic = install_and_start_script_for(predicted, false);
        assert!(automatic.contains("nohup yas server"));
        assert!(!automatic.contains("YAS_SOCK=\"$S\""));

        let explicit = install_and_start_script_for(predicted, true);
        assert!(explicit.contains("YAS_SOCK=\"$S\" nohup yas server"));
        assert!(explicit.contains(predicted), "explicit path stayed exact");
    }

    #[test]
    fn plain_remote_command_passes_only_paths_that_need_no_quoting() {
        assert_eq!(plain_remote_command(None).unwrap(), "yas connect --stdio");
        assert_eq!(
            plain_remote_command(Some("/run/user/1000/yas/yas-default.sock")).unwrap(),
            "yas --on socket:/run/user/1000/yas/yas-default.sock connect --stdio"
        );
        assert_eq!(
            plain_remote_command(Some(r"\\.\pipe\yas-default")).unwrap(),
            r"yas --on socket:\\.\pipe\yas-default connect --stdio"
        );
        for unsafe_path in ["/tmp/a b.sock", "/tmp/$(x).sock", "/tmp/a'b", ""] {
            assert!(
                plain_remote_command(Some(unsafe_path)).is_err(),
                "{unsafe_path}"
            );
        }
    }

    #[test]
    fn posix_remote_command_quotes_an_explicit_socket_through_both_shells() {
        let command = posix_remote_command(Some("/tmp/owner's-$(touch nope).sock"), false);
        assert!(command.starts_with("sh -c '"), "{command}");
        assert!(
            command.contains("owner'\"'\"'s-\\$(touch nope).sock"),
            "{command}"
        );
        assert!(
            command.ends_with("exec yas connect --stdio --no-start'"),
            "{command}"
        );
        let automatic = posix_remote_command(None, true);
        assert!(!automatic.contains("YAS_SOCK"), "{automatic}");
        assert!(
            automatic.ends_with("exec yas connect --stdio'"),
            "{automatic}"
        );
        assert!(automatic.contains("$HOME/.local/bin"), "{automatic}");
    }

    #[test]
    fn only_prohibited_or_unknown_channels_mean_forwarding_is_refused() {
        use russh::ChannelOpenFailure as Failure;
        let open = |failure| russh::Error::ChannelOpenFailure(failure);
        assert!(forwarding_refused(&open(
            Failure::AdministrativelyProhibited
        )));
        assert!(forwarding_refused(&open(Failure::UnknownChannelType)));
        assert!(!forwarding_refused(&open(Failure::ConnectFailed)));
        assert!(!forwarding_refused(&russh::Error::Disconnect));
    }

    #[test]
    fn explicit_start_quotes_a_single_quote_through_both_shells() {
        let path = "/tmp/owner's-$(touch nope).sock";
        let script = install_and_start_script_for(path, true);
        assert!(script.starts_with("sh -c '"));
        assert!(script.contains("owner'\"'\"'s-\\$(touch nope).sock"));
        assert!(script.contains("YAS_SOCK=\"$S\" nohup yas server"));
    }

    fn key(s: &str) -> keys::PublicKey {
        keys::PublicKey::from_openssh(s).expect("fixture parses")
    }

    /// Each test gets its own known_hosts, pointed at via the env override.
    /// The var is process-wide, so these run under one lock rather than in
    /// parallel.
    fn with_known_hosts<T>(initial: Option<&str>, f: impl FnOnce(&Path) -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = std::env::temp_dir().join(format!(
            "yas-ssh-kh-{}-{:p}",
            std::process::id(),
            &initial as *const _
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        match initial {
            Some(contents) => std::fs::write(&path, contents).unwrap(),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        unsafe { std::env::set_var("YAS_SSH_KNOWN_HOSTS", &path) };
        let out = f(&path);
        unsafe { std::env::remove_var("YAS_SSH_KNOWN_HOSTS") };
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn check(presented: &str) -> Result<bool, Error> {
        let mut h = SshHandler {
            host: "example.test".into(),
            port: 22,
            policy: HostKeyPolicy::KnownHosts,
        };
        futures_lite_block_on(h.check_server_key(&key(presented).into()))
    }

    /// Minimal executor: `check_server_key` never yields, so polling once is
    /// enough and the crate needs no async test runtime.
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(clone(std::ptr::null())) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("check_server_key awaited something"),
        }
    }

    #[test]
    fn unknown_host_is_learned_then_enforced() {
        with_known_hosts(None, |path| {
            assert!(check(KEY_A).unwrap(), "first sight is trusted");
            let recorded = std::fs::read_to_string(path).unwrap();
            assert!(recorded.contains("example.test"), "key was recorded");
            assert!(check(KEY_A).unwrap(), "same key still accepted");
        });
    }

    /// The whole point of pinning: a different key for a pinned host is
    /// refused rather than appended.
    #[test]
    fn a_changed_key_is_refused() {
        with_known_hosts(Some(&format!("example.test {KEY_A}\n")), |_| {
            let err = check(KEY_B).expect_err("changed key must not be accepted");
            assert!(format!("{err}").contains("does not match"), "{err}");
        });
    }

    /// russh answers `Ok(false)` for a host pinned under another algorithm,
    /// which is indistinguishable from an unknown host. Appending on that —
    /// as this used to — let an attacker bypass an ed25519 pin by presenting
    /// an RSA key.
    #[test]
    fn a_different_algorithm_does_not_bypass_the_pin() {
        with_known_hosts(Some(&format!("example.test {KEY_A}\n")), |path| {
            let err = check(KEY_RSA).expect_err("algorithm switch must not be accepted");
            assert!(format!("{err}").contains("does not match"), "{err}");
            let after = std::fs::read_to_string(path).unwrap();
            assert!(
                !after.contains("ssh-rsa"),
                "must not record the rejected key"
            );
        });
    }

    /// A corrupt entry means the pin could not be read, not that there is
    /// none. This arm used to append the presented key and accept.
    #[test]
    fn a_corrupt_entry_for_this_host_is_fatal() {
        with_known_hosts(
            Some("example.test ssh-ed25519 !!!not-base64!!!\n"),
            |path| {
                let err = check(KEY_A).expect_err("unreadable pin must not be accepted");
                assert!(format!("{err}").contains("cannot parse"), "{err}");
                let after = std::fs::read_to_string(path).unwrap();
                assert!(!after.contains(KEY_A), "must not record over a corrupt pin");
            },
        );
    }

    /// An entry for a *different* host must not pin this one, but must also
    /// not stop this one being learned.
    #[test]
    fn another_hosts_entry_does_not_pin_this_one() {
        with_known_hosts(Some(&format!("other.test {KEY_B}\n")), |path| {
            assert!(check(KEY_A).unwrap(), "unrelated entry does not pin us");
            let after = std::fs::read_to_string(path).unwrap();
            assert!(after.contains("other.test"), "existing entry preserved");
            assert!(after.contains("example.test"), "new entry appended");
        });
    }

    /// The previous append hand-rolled the write and never checked for a
    /// trailing newline, so a file not ending in one had its last entry
    /// corrupted by concatenation.
    #[test]
    fn learning_does_not_corrupt_a_file_without_a_trailing_newline() {
        with_known_hosts(Some(&format!("other.test {KEY_B}")), |path| {
            assert!(check(KEY_A).unwrap());
            let after = std::fs::read_to_string(path).unwrap();
            let lines: Vec<_> = after.lines().filter(|l| !l.trim().is_empty()).collect();
            assert_eq!(
                lines.len(),
                2,
                "entries stayed on separate lines: {after:?}"
            );
            assert!(lines[0].starts_with("other.test"));
            assert!(lines[1].starts_with("example.test"));
        });
    }

    fn check_with(policy: HostKeyPolicy, presented: &str) -> Result<bool, Error> {
        let mut h = SshHandler {
            host: "example.test".into(),
            port: 2222,
            policy,
        };
        futures_lite_block_on(h.check_server_key(&key(presented).into()))
    }

    #[test]
    fn pinned_keys_accept_only_themselves_and_touch_no_file() {
        with_known_hosts(None, |path| {
            let pinned = HostKeyPolicy::Pinned(vec![parse_public_key(KEY_A).unwrap()]);
            assert!(check_with(pinned.clone(), KEY_A).unwrap());
            let err = check_with(pinned.clone(), KEY_B).expect_err("unpinned key");
            assert!(
                format!("{err}").contains("not one of the 1 pinned"),
                "{err}"
            );
            assert!(check_with(pinned, KEY_RSA).is_err());
            let commented =
                HostKeyPolicy::Pinned(vec![parse_public_key(&format!("{KEY_A} me@host")).unwrap()]);
            assert!(
                check_with(commented, KEY_A).unwrap(),
                "comments are not identity"
            );
            assert!(!path.exists(), "pinning must not write known_hosts");
        });
    }

    #[test]
    fn verify_callback_sees_host_port_and_fingerprint() {
        let expected = fingerprint(&key(KEY_A));
        let policy = HostKeyPolicy::Verify(Arc::new(move |check: &HostKeyCheck<'_>| {
            check.host == "example.test" && check.port == 2222 && check.fingerprint() == expected
        }));
        assert!(check_with(policy.clone(), KEY_A).unwrap());
        assert!(check_with(policy, KEY_B).is_err());
    }

    #[test]
    fn public_keys_parse_from_every_common_form() {
        let bare = KEY_A.split_whitespace().nth(1).unwrap();
        let a = parse_public_key(KEY_A).unwrap();
        let data = |text: &str| parse_public_key(text).unwrap().key_data().clone();
        assert_eq!(data(&format!("{KEY_A} me@host")), *a.key_data());
        assert_eq!(data(&format!("example.test {KEY_A}")), *a.key_data());
        assert_eq!(data(bare), *a.key_data());
        assert!(fingerprint(&a).starts_with("SHA256:"));
        assert_eq!(public_key_openssh(&a), KEY_A);
    }

    /// A throwaway key generated for this test only.
    const TEST_PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\nQyNTUxOQAAACCgwmK3xYT+8Vx7J/rtA3C9cN3wlVh5V30Yyif+grok4gAAAJD1M1av9TNW\nrwAAAAtzc2gtZWQyNTUxOQAAACCgwmK3xYT+8Vx7J/rtA3C9cN3wlVh5V30Yyif+grok4g\nAAAEC8hSGyHNl8/0lO+hkgzJEZEkENc4U0+GcCfUiDHc1e36DCYrfFhP7xXHsn+u0DcL1w\n3fCVWHlXfRjKJ/6CuiTiAAAADHlhcy1zc2ggdGVzdAE=\n-----END OPENSSH PRIVATE KEY-----\n";
    const TEST_PUBLIC_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKDCYrfFhP7xXHsn+u0DcL1w3fCVWHlXfRjKJ/6CuiTi yas-ssh test";

    #[test]
    fn in_memory_private_keys_load_from_text() {
        let options = SshOptions::in_memory(HostKeyPolicy::Pinned(Vec::new()))
            .with_private_key(TEST_PRIVATE_KEY, None)
            .unwrap();
        assert_eq!(options.keys.len(), 1);
        assert!(!options.agent && !options.key_files && !options.config_file);
        assert_eq!(
            options.keys[0].public_key(),
            &parse_public_key(TEST_PUBLIC_KEY).unwrap()
        );
        assert!(
            SshOptions::default()
                .with_private_key("not a key", None)
                .is_err()
        );
    }

    #[test]
    fn russh_reports_an_algorithm_mismatch_as_unknown_host() {
        // The premise behind `a_different_algorithm_does_not_bypass_the_pin`:
        // russh cannot distinguish these two for us, so we must.
        with_known_hosts(Some(&format!("example.test {KEY_A}\n")), |path| {
            // Same algorithm, different key -> KeyChanged (russh catches it).
            let e = keys::check_known_hosts_path("example.test", 22, &key(KEY_B), path);
            assert!(
                matches!(e, Err(keys::Error::KeyChanged { .. })),
                "expected KeyChanged, got {e:?}"
            );
            // Different algorithm -> Ok(false), i.e. indistinguishable from
            // a host that was never pinned at all.
            let e = keys::check_known_hosts_path("example.test", 22, &key(KEY_RSA), path);
            assert!(
                matches!(e, Ok(false)),
                "expected Ok(false) — the bypass — got {e:?}"
            );
        });
    }
}
