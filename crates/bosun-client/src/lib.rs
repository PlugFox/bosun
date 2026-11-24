//! The Rust client of the bosun local API (#4, #24), used by the CLI, the TUI and the MCP
//! server.
//!
//! [`Client::connect`] opens the [`Endpoint`], calls `hello` and refuses a daemon whose API
//! version is not in [`SUPPORTED_API_VERSIONS`], saying which side to upgrade. Then
//! [`Client::call`] sends one request at a time and waits for its response.
//!
//! [`Subscription`] follows an identity's event stream and reconnects by itself from the last
//! sequence it delivered.

pub mod endpoint;
pub mod framing;
pub mod operator;
mod subscribe;

use std::fmt;
use std::io;
use std::time::Duration;

use bosun_api::rpc::{self, ErrorObject, Request, Response};
use bosun_api::{API_VERSION, HelloParams, HelloResult, Method, method};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadHalf, WriteHalf};

pub use endpoint::Endpoint;
pub use subscribe::{Cursor, Delivery, Subscription};

/// The API versions this client speaks.
pub const SUPPORTED_API_VERSIONS: &[u32] = &[API_VERSION];

/// How long [`Client::connect`] waits for the endpoint and for `hello`.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What can go wrong talking to the daemon.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Nothing listens at the endpoint.
    NotRunning {
        endpoint: String,
    },
    /// The daemon speaks an API version this client does not know.
    UnsupportedVersion {
        daemon: u32,
    },
    /// The daemon answered the request with an error.
    Rpc(ErrorObject),
    /// The daemon sent something that is not a valid response.
    Protocol(String),
    /// The daemon closed the connection.
    Closed,
    /// The daemon did not answer in time.
    Timeout,
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning { endpoint } => {
                write!(
                    f,
                    "the daemon is not running (nothing listens at {endpoint})"
                )
            }
            Self::UnsupportedVersion { daemon } => {
                let known = SUPPORTED_API_VERSIONS
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let upgrade = if SUPPORTED_API_VERSIONS.iter().all(|v| v < daemon) {
                    "upgrade bosun (the client)"
                } else {
                    "upgrade bosund (the daemon)"
                };
                write!(
                    f,
                    "the daemon speaks API version {daemon} and this client knows {known}: \
                     {upgrade}"
                )
            }
            Self::Rpc(e) => write!(
                f,
                "the daemon refused the request: {} ({})",
                e.message, e.code
            ),
            Self::Protocol(what) => write!(f, "protocol error: {what}"),
            Self::Closed => f.write_str("the daemon closed the connection"),
            Self::Timeout => f.write_str("the daemon did not answer in time"),
            Self::Io(e) => write!(f, "endpoint: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Refuses an API version this client does not know.
///
/// # Errors
///
/// [`Error::UnsupportedVersion`] when `daemon` is not in [`SUPPORTED_API_VERSIONS`].
pub fn check_version(daemon: u32) -> Result<(), Error> {
    if SUPPORTED_API_VERSIONS.contains(&daemon) {
        Ok(())
    } else {
        Err(Error::UnsupportedVersion { daemon })
    }
}

/// A byte stream to the daemon.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// Opens the endpoint. Nothing is sent.
///
/// # Errors
///
/// [`Error::NotRunning`] when nothing listens there; [`Error::Io`] otherwise.
pub async fn open(endpoint: &Endpoint) -> Result<Box<dyn Stream>, Error> {
    let not_running = |e: &io::Error| {
        matches!(
            e.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    };
    match open_stream(endpoint).await {
        Ok(stream) => Ok(stream),
        Err(e) if not_running(&e) => Err(Error::NotRunning {
            endpoint: endpoint.to_string(),
        }),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(unix)]
async fn open_stream(endpoint: &Endpoint) -> io::Result<Box<dyn Stream>> {
    Ok(Box::new(
        tokio::net::UnixStream::connect(endpoint.path()).await?,
    ))
}

#[cfg(windows)]
async fn open_stream(endpoint: &Endpoint) -> io::Result<Box<dyn Stream>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    /// `ERROR_PIPE_BUSY`: every instance of the pipe is connected; the server makes another.
    const PIPE_BUSY: i32 = 231;
    loop {
        match ClientOptions::new().open(endpoint.path()) {
            Ok(client) => return Ok(Box::new(client)),
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// The read side of a connection, buffered.
pub(crate) type Reader = BufReader<ReadHalf<Box<dyn Stream>>>;
/// The write side of a connection.
pub(crate) type Writer = WriteHalf<Box<dyn Stream>>;

/// A connection to the daemon that has passed `hello`.
pub struct Client {
    reader: Reader,
    writer: Writer,
    next_id: u64,
    hello: HelloResult,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("next_id", &self.next_id)
            .field("hello", &self.hello)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Opens `endpoint` and calls `hello`, within [`CONNECT_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// [`Error::NotRunning`], [`Error::Timeout`], [`Error::UnsupportedVersion`], or a failure
    /// of the call.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self, Error> {
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            Self::over(open(endpoint).await?).await
        })
        .await
        .map_err(|_| Error::Timeout)?
    }

    /// Calls `hello` over an open stream and checks the version.
    ///
    /// # Errors
    ///
    /// As [`Client::connect`], without the timeout.
    pub async fn over(stream: Box<dyn Stream>) -> Result<Self, Error> {
        let (read, writer) = tokio::io::split(stream);
        let mut client = Self {
            reader: BufReader::new(read),
            writer,
            next_id: 1,
            hello: HelloResult {
                api_version: 0,
                daemon_version: String::new(),
                pid: 0,
            },
        };
        client.hello = client
            .call::<method::Hello>(&HelloParams::default())
            .await?;
        check_version(client.hello.api_version)?;
        Ok(client)
    }

    /// What the daemon answered to `hello`.
    #[must_use]
    pub fn daemon(&self) -> &HelloResult {
        &self.hello
    }

    /// The open stream, buffered as it was read so far.
    pub(crate) fn into_parts(self) -> (Reader, Writer) {
        (self.reader, self.writer)
    }

    /// Calls method `M` and waits for its result.
    ///
    /// # Errors
    ///
    /// [`Error::Rpc`] when the daemon refused the request; I/O and protocol errors otherwise.
    pub async fn call<M: Method>(&mut self, params: &M::Params) -> Result<M::Result, Error> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request {
            jsonrpc: rpc::JSONRPC.to_owned(),
            id: Some(id.into()),
            method: M::NAME.to_owned(),
            params: serde_json::to_value(params)
                .map_err(|e| Error::Protocol(format!("params of {}: {e}", M::NAME)))?,
        };
        framing::write_message(&mut self.writer, &request).await?;
        let line = framing::read_line(&mut self.reader)
            .await?
            .ok_or(Error::Closed)?;
        let response: Response = serde_json::from_str(&line)
            .map_err(|e| Error::Protocol(format!("a response that does not parse: {e}")))?;
        if response.id != id {
            return Err(Error::Protocol(format!(
                "a response to request {} while waiting for {id}",
                response.id
            )));
        }
        match (response.result, response.error) {
            (_, Some(error)) => Err(Error::Rpc(error)),
            (Some(result), None) => serde_json::from_value(result)
                .map_err(|e| Error::Protocol(format!("the result of {}: {e}", M::NAME))),
            (None, None) => Err(Error::Protocol("a response with no result".to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use bosun_api::rpc::Request;
    use tokio::io::AsyncWriteExt;

    use super::*;

    /// A fake daemon on one end of a pipe that answers `hello` with `api_version`.
    async fn handshake(api_version: u32) -> Result<Client, Error> {
        let (ours, theirs) = tokio::io::duplex(4096);
        let daemon = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(theirs);
            let mut read = BufReader::new(read);
            let line = framing::read_line(&mut read).await.unwrap().unwrap();
            let request: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(request.method, "hello");
            let hello = HelloResult {
                api_version,
                daemon_version: "0.0.0".to_owned(),
                pid: 1,
            };
            let response =
                Response::result(request.id.unwrap(), serde_json::to_value(hello).unwrap());
            framing::write_message(&mut write, &response).await.unwrap();
            write.flush().await.unwrap();
            // Hold the stream open until the client is done with it.
            let _ = framing::read_line(&mut read).await;
        });
        let client = Client::over(Box::new(ours)).await;
        daemon.abort();
        client
    }

    #[tokio::test]
    async fn hello_returns_the_api_version() {
        let client = handshake(API_VERSION).await.unwrap();
        assert_eq!(client.daemon().api_version, API_VERSION);
    }

    #[tokio::test]
    async fn an_unknown_major_version_is_refused() {
        for version in [0, API_VERSION + 1, 99] {
            let err = handshake(version).await.unwrap_err();
            assert!(
                matches!(err, Error::UnsupportedVersion { daemon } if daemon == version),
                "{err}"
            );
        }
        let newer = Error::UnsupportedVersion {
            daemon: API_VERSION + 1,
        };
        assert!(newer.to_string().contains("upgrade bosun (the client)"));
        let older = Error::UnsupportedVersion { daemon: 0 };
        assert!(older.to_string().contains("upgrade bosund (the daemon)"));
    }
}
