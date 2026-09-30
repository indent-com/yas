//! A minimal SSH server for tests: it accepts one client key, forwards
//! `direct-streamlocal` channels to Unix sockets (or refuses them, like an
//! sshd with `AllowStreamLocalForwarding no`), and runs exec requests with
//! `sh -c` on this machine, piping the channel to the command's stdin and
//! stdout. It records what it was asked for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure};
use tokio::io::AsyncWriteExt;

/// A deterministic Ed25519 key, so no key material lives in the tree.
pub fn key(seed: u8) -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

#[derive(Debug, Default)]
pub struct Requests {
    pub streamlocal: Vec<String>,
    pub exec: Vec<String>,
}

pub struct Options {
    pub client_key: PublicKey,
    /// Forward `direct-streamlocal` channels; otherwise refuse them.
    pub streamlocal: bool,
    /// Environment for exec'd commands, on top of this process's.
    pub env: Vec<(String, String)>,
}

pub struct TestSshServer {
    pub port: u16,
    pub host_key: PublicKey,
    pub requests: Arc<Mutex<Requests>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestSshServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestSshServer {
    pub async fn start(options: Options) -> Self {
        let host = key(0x11);
        let host_key = host.public_key().clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(server::Config {
            keys: vec![host],
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let options = Arc::new(options);
        let requests = Arc::new(Mutex::new(Requests::default()));
        let shared = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let handler = Handler {
                    options: Arc::clone(&options),
                    requests: Arc::clone(&shared),
                    channels: HashMap::new(),
                };
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, tcp, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            port,
            host_key,
            requests,
            task,
        }
    }
}

struct Handler {
    options: Arc<Options>,
    requests: Arc<Mutex<Requests>>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl server::Handler for Handler {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if key.key_data() == self.options.client_key.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_open_direct_streamlocal(
        &mut self,
        channel: Channel<Msg>,
        socket_path: &str,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.requests
            .lock()
            .unwrap()
            .streamlocal
            .push(socket_path.to_owned());
        if !self.options.streamlocal {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        match tokio::net::UnixStream::connect(socket_path).await {
            Ok(unix) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let mut unix = unix;
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut unix).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        self.requests.lock().unwrap().exec.push(command.clone());
        let Some(channel) = self.channels.remove(&id) else {
            session.channel_failure(id)?;
            return Ok(());
        };
        let spawned = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .envs(self.options.env.iter().cloned())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = spawned else {
            session.channel_failure(id)?;
            return Ok(());
        };
        session.channel_success(id)?;
        let handle = session.handle();
        tokio::spawn(async move {
            let (mut from_client, mut to_client) = tokio::io::split(channel.into_stream());
            let mut stdin = child.stdin.take().unwrap();
            let mut stdout = child.stdout.take().unwrap();
            // Client EOF closes the command's stdin; the command's exit ends
            // the channel whether or not the client ever sent EOF.
            let input = tokio::spawn(async move {
                let _ = tokio::io::copy(&mut from_client, &mut stdin).await;
            });
            let _ = tokio::io::copy(&mut stdout, &mut to_client).await;
            let _ = to_client.shutdown().await;
            let status = child.wait().await.ok().and_then(|status| status.code());
            input.abort();
            let _ = handle
                .exit_status_request(id, status.unwrap_or(255) as u32)
                .await;
            let _ = handle.close(id).await;
        });
        Ok(())
    }
}
