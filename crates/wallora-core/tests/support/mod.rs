//! Loopback HTTP helpers shared by provider integration tests.
//!
//! These helpers speak just enough HTTP/1.1 to exercise the provider's image transport seam:
//! explicit headers, withheld bodies, chunked bodies and connection concurrency. They never
//! touch the network beyond `127.0.0.1`.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Upper bound for reading one request head; keeps a stalled client from wedging a test.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum accepted request head size.
pub const MAX_HEAD: usize = 64 * 1024;

/// Binds an ephemeral loopback listener and returns it with its address.
pub async fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener address");
    (listener, addr)
}

/// Spawns an accept loop that runs `handler` once per connection.
pub fn serve<H, F>(listener: TcpListener, handler: H) -> tokio::task::JoinHandle<()>
where
    H: Fn(TcpStream) -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        while let Ok((stream, _peer)) = listener.accept().await {
            tokio::spawn(handler(stream));
        }
    })
}

/// Reads one request head (through the blank line) and returns it as lossy UTF-8.
pub async fn read_request_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut byte))
            .await
            .map_err(|_| io::Error::other("timed out reading request head"))??;
        if read == 0 {
            break;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_HEAD {
            return Err(io::Error::other("request head too large"));
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Writes a raw response head, including the terminating blank line.
pub async fn write_head(stream: &mut TcpStream, headers: &[(&str, &str)]) -> io::Result<()> {
    let mut head = String::from("HTTP/1.1 200 OK\r\n");
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await
}

/// Writes one HTTP chunk (`hex length`, CRLF, data, CRLF).
pub async fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    stream
        .write_all(format!("{:x}\r\n", data.len()).as_bytes())
        .await?;
    stream.write_all(data).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

/// Terminates a chunked body.
pub async fn finish_chunks(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await
}
