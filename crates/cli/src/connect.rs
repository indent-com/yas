//! `yas connect --stdio`: this process's stdin and stdout carry one native
//! YAS session with a server, so anything that can run a command with pipes
//! (an SSH exec channel, `docker exec -i`, a child process) reaches YAS.
//!
//! The bytes are relayed as they are: the peer on the pipes runs the whole
//! YAS handshake itself. Stdout carries nothing else.

use std::io::{ErrorKind, IsTerminal};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bytes read at once in either direction (one hard-maximum wire frame is
/// larger; it simply takes several reads).
const BUFFER: usize = 256 * 1024;

/// Relay stdin/stdout to the server `on` names (the configured default, else
/// the local server, started unless `start` is false). Returns the exit code.
pub async fn cmd_connect_stdio(on: Option<&str>, hub: &str, start: bool) -> Result<i32, String> {
    // A terminal's line discipline would mangle the binary stream (and the
    // stream would mangle the terminal).
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return Err(
            "connect --stdio speaks the YAS protocol on stdin and stdout; \
             run it from a program (ssh host yas connect --stdio, docker exec -i …), \
             not a terminal"
                .to_owned(),
        );
    }
    let mut options = crate::transport::cli_options(hub);
    options.start_local = start;
    let transport = crate::transport::connect_target(on, &options).await?;
    let (reader, writer) = transport.split();
    relay(tokio::io::stdin(), tokio::io::stdout(), reader, writer).await
}

/// Relay `input` to `upstream` and `downstream` to `output` until the server
/// side ends. When `input` ends first the server is told (write shutdown)
/// and its remaining bytes are still delivered.
async fn relay<I, O, R, W>(
    input: I,
    output: O,
    downstream: R,
    mut upstream: W,
) -> Result<i32, String>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let to_server = async move {
        let result = pump(input, &mut upstream).await;
        let _ = upstream.shutdown().await;
        result
    };
    let from_server = pump(downstream, output);
    tokio::pin!(from_server);
    tokio::select! {
        result = &mut from_server => finish("from the server", result),
        result = to_server => {
            if let Err(error) = result
                && !peer_gone(&error)
            {
                eprintln!("yas: connect --stdio: stdin: {error}");
            }
            finish("from the server", from_server.await)
        }
    }
}

fn finish(direction: &str, result: std::io::Result<()>) -> Result<i32, String> {
    match result {
        Ok(()) => Ok(0),
        // Whoever reads our stdout went away: nothing is left to deliver to.
        Err(error) if peer_gone(&error) => Ok(0),
        Err(error) => Err(format!("connect --stdio: relaying {direction}: {error}")),
    }
}

fn peer_gone(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
    )
}

/// Copy until EOF, flushing after every chunk: stdout is line-buffered and a
/// YAS frame has no reason to end in a newline, so an unflushed reply could
/// wait forever for the next one.
async fn pump<R, W>(mut reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; BUFFER];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return writer.flush().await;
        }
        writer.write_all(&buffer[..count]).await?;
        writer.flush().await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bytes_flow_both_ways_and_input_end_reaches_the_server() {
        let (client_in, mut feed) = tokio::io::duplex(64);
        let (mut observe, client_out) = tokio::io::duplex(64);
        let (server, mut server_peer) = tokio::io::duplex(64);
        let (server_reader, server_writer) = tokio::io::split(server);
        let relay = tokio::spawn(relay(client_in, client_out, server_reader, server_writer));

        feed.write_all(b"hello").await.unwrap();
        drop(feed);
        // The server sees the input, then its end.
        let mut seen = Vec::new();
        server_peer.read_to_end(&mut seen).await.unwrap();
        assert_eq!(seen, b"hello");
        // It still answers after the input ended; the relay ends with it.
        server_peer.write_all(b"world").await.unwrap();
        drop(server_peer);
        let mut answered = Vec::new();
        observe.read_to_end(&mut answered).await.unwrap();
        assert_eq!(answered, b"world");
        assert_eq!(relay.await.unwrap(), Ok(0));
    }
}
