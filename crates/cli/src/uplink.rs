//! `yas uplink` — expose the local yas server through a relay.
//!
//! Authenticates to an HTTPS control endpoint with `YAS_UPLINK_TOKEN`,
//! receives a pool of relays (WebTransport, WebSocket), holds a session with
//! one, authenticates each consumer using pinned X25519 keys over Noise IK,
//! then bridges decrypted streams to local YAS. The producer itself is
//! [`yas_proxy::uplink_producer`]; this is its command line.

use yas_proxy::uplink_producer::{Local, Producer, Transport};

/// Derive only the public identity; private input never appears in diagnostics.
pub fn public_key_from_env() -> Result<yas_uplink::PublicKey, String> {
    let encoded = std::env::var("YAS_UPLINK_IDENTITY").map_err(|error| match error {
        std::env::VarError::NotPresent =>
            "YAS_UPLINK_IDENTITY is not set; load your existing private key or generate one with yas uplink-keygen --private".to_owned(),
        std::env::VarError::NotUnicode(_) =>
            "YAS_UPLINK_IDENTITY must be 43 characters of unpadded base64url".to_owned(),
    })?;
    yas_uplink::Identity::from_base64(&encoded)
        .map(|identity| identity.public_key())
        .map_err(|error| format!("YAS_UPLINK_IDENTITY: {error}"))
}

/// Build a consumer URI offline, with a locally derived server pin. The client
/// token is distinct from the producer token; only the relay can issue it.
pub fn connection_url(control: &str, client_token: &str) -> Result<String, String> {
    let mut url = url::Url::parse(control).map_err(|_| "invalid uplink control URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "uplink control URL must be HTTPS without userinfo, query parameters, or a fragment"
                .into(),
        );
    }
    if client_token.trim().is_empty() || client_token.chars().any(char::is_control) {
        return Err("client token must be nonempty and contain no control characters".into());
    }
    let public = public_key_from_env()?;
    let fragment = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", client_token)
        .append_pair("server", &public.to_string())
        .finish();
    url.set_fragment(Some(&fragment));
    Ok(format!("uplink:{url}"))
}

/// Publish the local server until a fatal error (the control endpoint
/// refusing the token) or Ctrl-C, which closes the relay session.
pub async fn cmd_uplink(
    url: String,
    identity: String,
    allow_client: Vec<String>,
    transport: Option<String>,
) -> Result<(), String> {
    let identity = yas_uplink::Identity::from_base64(&identity)?;
    let allowed = allow_client
        .iter()
        .map(|key| key.parse())
        .collect::<Result<Vec<_>, _>>()?;
    let crypto = identity.server_config(allowed)?;
    let token = std::env::var("YAS_UPLINK_TOKEN").unwrap_or_default();
    let transport: Transport = match transport {
        Some(transport) => transport.parse()?,
        None => Transport::default(),
    };
    let socket = crate::transport::default_local_socket();
    let local = Local::custom(socket.clone(), move || {
        let socket = socket.clone();
        async move {
            let transport = crate::transport::connect_ipc(&socket)
                .await
                .map_err(std::io::Error::other)?;
            let (reader, writer) = transport.split();
            Ok(tokio::io::join(reader, writer))
        }
    });
    Producer::new(&url, &token, crypto, local)?
        .transport(transport)
        .on_event(|event| eprintln!("[uplink] {event}"))
        .run_until(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
