//! LSP transport layer for JSON-RPC communication
//!
//! Handles the wire protocol for communicating with language servers via stdio.
//! The LSP uses JSON-RPC 2.0 with HTTP-style headers for framing.

use std::io;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, Mutex};

/// Errors that can occur during LSP transport operations
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Missing Content-Length header")]
    MissingContentLength,

    #[error("Invalid Content-Length: {0}")]
    InvalidContentLength(String),

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Invalid UTF-8 in message")]
    InvalidUtf8,
}

/// Whether an error is just the stream ending rather than something wrong.
///
/// A server exiting closes its stdout, so the reader sees EOF; anything still in
/// flight to a departed server gets `BrokenPipe`. Both are how every shutdown
/// ends, including a deliberate one. Logging them as errors printed
///
/// ```text
/// LSP reader error: I/O error: EOF while reading headers
/// LSP writer error: I/O error: Broken pipe (os error 32)
/// ```
///
/// on every project switch, directly above the message that said what had
/// actually gone wrong — so the real failure read as the third line of a
/// cascade rather than as the cause.
fn is_stream_end(error: &TransportError) -> bool {
    match error {
        TransportError::Io(e) => matches!(
            e.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

/// LSP transport for JSON-RPC over stdio
pub struct LspTransport {
    writer_tx: Mutex<Option<mpsc::Sender<String>>>,
}

impl LspTransport {
    /// Create a new transport from process stdin/stdout
    ///
    /// Spawns background tasks to handle reading and writing
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<String>) {
        // Channel for incoming messages (from server)
        let (incoming_tx, incoming_rx) = mpsc::channel::<String>(64);
        // Channel for reader task to forward messages
        // Channel for outgoing messages (to server)
        let (writer_tx, writer_rx) = mpsc::channel::<String>(64);

        // Spawn reader task
        let incoming_tx_clone = incoming_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = Self::reader_loop(stdout, incoming_tx_clone).await {
                if !is_stream_end(&e) {
                    eprintln!("LSP reader error: {}", e);
                }
            }
        });

        // Spawn writer task
        tokio::spawn(async move {
            if let Err(e) = Self::writer_loop(stdin, writer_rx).await {
                if !is_stream_end(&e) {
                    eprintln!("LSP writer error: {}", e);
                }
            }
        });

        (
            Self {
                writer_tx: Mutex::new(Some(writer_tx)),
            },
            incoming_rx,
        )
    }

    /// Send a message to the language server
    pub async fn send(&self, message: String) -> Result<(), TransportError> {
        // Clone under the transport guard and release it before channel
        // backpressure can suspend this task. Shutdown must always be able to
        // take the sender even while another request is waiting to write.
        let writer = self
            .writer_tx
            .lock()
            .await
            .clone()
            .ok_or(TransportError::ChannelClosed)?;
        writer
            .send(message)
            .await
            .map_err(|_| TransportError::ChannelClosed)
    }

    /// Close the server's stdin after queued messages have been written.
    pub async fn close_writer(&self) {
        self.writer_tx.lock().await.take();
    }

    /// Read loop: reads messages from stdout and forwards to channel
    async fn reader_loop(
        stdout: ChildStdout,
        tx: mpsc::Sender<String>,
    ) -> Result<(), TransportError> {
        let mut reader = BufReader::new(stdout);

        loop {
            // Read headers
            let content_length = Self::read_headers(&mut reader).await?;

            // Read content
            let mut content = vec![0u8; content_length];
            reader.read_exact(&mut content).await?;

            let message = String::from_utf8(content).map_err(|_| TransportError::InvalidUtf8)?;

            if tx.send(message).await.is_err() {
                break;
            }
        }

        Ok(())
    }

    /// Read LSP headers and return Content-Length
    pub(crate) async fn read_headers<R: AsyncBufReadExt + Unpin>(
        reader: &mut R,
    ) -> Result<usize, TransportError> {
        let mut content_length: Option<usize> = None;

        loop {
            let mut line = String::new();
            let bytes_read = reader.read_line(&mut line).await?;

            if bytes_read == 0 {
                return Err(TransportError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF while reading headers",
                )));
            }

            // Empty line marks end of headers
            if line == "\r\n" || line == "\n" {
                break;
            }

            // Parse Content-Length header
            let line = line.trim();
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                content_length = Some(
                    value
                        .parse()
                        .map_err(|_| TransportError::InvalidContentLength(value.to_string()))?,
                );
            }
            // Ignore other headers (Content-Type, etc.)
        }

        content_length.ok_or(TransportError::MissingContentLength)
    }

    /// Write loop: reads messages from channel and sends to stdin
    async fn writer_loop(
        mut stdin: ChildStdin,
        mut rx: mpsc::Receiver<String>,
    ) -> Result<(), TransportError> {
        while let Some(message) = rx.recv().await {
            Self::write_message(&mut stdin, &message).await?;
        }
        Ok(())
    }

    /// Write a message with LSP framing
    pub(crate) async fn write_message<W: AsyncWriteExt + Unpin>(
        writer: &mut W,
        content: &str,
    ) -> Result<(), TransportError> {
        let header = format!("Content-Length: {}\r\n\r\n", content.len());
        writer.write_all(header.as_bytes()).await?;
        writer.write_all(content.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Framing is the wire protocol: get the byte count wrong and the server
    /// reads a truncated message and hangs. These drive the *live* async path
    /// rather than a parallel copy, which is what the previous tests did before
    /// their helpers were deleted as unused.

    #[tokio::test]
    async fn writes_a_correctly_framed_message() {
        let mut buffer = Vec::new();
        LspTransport::write_message(&mut buffer, r#"{"jsonrpc":"2.0"}"#)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            "Content-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}"
        );
    }

    /// The length is in bytes, not characters — a multi-byte payload framed by
    /// character count is silently truncated on the wire.
    #[tokio::test]
    async fn the_content_length_counts_bytes_not_characters() {
        let payload = r#"{"s":"é🦀"}"#;
        let mut buffer = Vec::new();
        LspTransport::write_message(&mut buffer, payload)
            .await
            .unwrap();
        let written = String::from_utf8(buffer).unwrap();
        assert!(written.starts_with(&format!("Content-Length: {}\r\n", payload.len())));
        assert!(payload.len() > payload.chars().count());
    }

    #[tokio::test]
    async fn reads_the_content_length_header() {
        let input = "Content-Length: 17\r\n\r\n";
        let mut reader = tokio::io::BufReader::new(input.as_bytes());
        assert_eq!(LspTransport::read_headers(&mut reader).await.unwrap(), 17);
    }

    /// Servers send headers we do not care about; they must be skipped, not
    /// treated as an error.
    #[tokio::test]
    async fn ignores_unknown_headers() {
        let input = "Content-Length: 17\r\nContent-Type: application/json\r\n\r\n";
        let mut reader = tokio::io::BufReader::new(input.as_bytes());
        assert_eq!(LspTransport::read_headers(&mut reader).await.unwrap(), 17);
    }

    #[tokio::test]
    async fn a_missing_content_length_is_an_error() {
        let input = "Content-Type: application/json\r\n\r\n";
        let mut reader = tokio::io::BufReader::new(input.as_bytes());
        assert!(matches!(
            LspTransport::read_headers(&mut reader).await,
            Err(TransportError::MissingContentLength)
        ));
    }

    #[tokio::test]
    async fn a_non_numeric_content_length_is_an_error() {
        let input = "Content-Length: banana\r\n\r\n";
        let mut reader = tokio::io::BufReader::new(input.as_bytes());
        assert!(matches!(
            LspTransport::read_headers(&mut reader).await,
            Err(TransportError::InvalidContentLength(_))
        ));
    }

    /// This is the failure that made a missing rust-analyzer look like our bug:
    /// the server exits instantly and the reader sees EOF mid-header.
    #[tokio::test]
    async fn an_immediate_eof_is_reported_as_such() {
        let mut reader = tokio::io::BufReader::new(&b""[..]);
        let err = LspTransport::read_headers(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("EOF") || err.to_string().contains("headers"));
    }
}
