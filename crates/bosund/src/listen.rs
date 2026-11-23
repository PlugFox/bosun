//! The daemon's side of the endpoint (#4, #24): one instance per endpoint, a Unix socket of
//! mode 0600 or a Windows named pipe.
//!
//! [`claim`] first asks whether a daemon already answers at the endpoint: something that
//! accepts the connection is a running instance, and this one must exit. Only when nothing
//! listens does it bind. On Unix a socket file left by a daemon that died is then stale and is
//! removed; on Windows a pipe vanishes with its last handle, and the first instance is created
//! with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a race between two starts is lost by one of them.

use std::io;

use bosun_client::{Client, Endpoint, Error as ClientError, Stream};

/// Why the endpoint could not be claimed.
#[derive(Debug)]
pub(crate) enum ClaimError {
    /// Another daemon listens there; its pid when it answered `hello`.
    Running(Option<u32>),
    /// The endpoint could not be probed or bound.
    Io(String),
}

/// Claims `endpoint` for this process, or says why not.
pub(crate) async fn claim(endpoint: &Endpoint) -> Result<Listener, ClaimError> {
    if !cfg!(mutant_api_skip_instance_check) {
        match bosun_client::open(endpoint).await {
            Ok(stream) => return Err(ClaimError::Running(pid_of(stream).await)),
            Err(ClientError::NotRunning { .. }) => {}
            Err(e) => return Err(ClaimError::Io(format!("{endpoint}: {e}"))),
        }
    }
    Listener::bind(endpoint).map_err(|e| {
        if e.kind() == io::ErrorKind::PermissionDenied && cfg!(windows) {
            // The first instance of the pipe exists: a daemon that has not answered yet.
            ClaimError::Running(None)
        } else {
            ClaimError::Io(format!("{endpoint}: {e}"))
        }
    })
}

/// The running instance's pid, if it answers `hello` in time.
async fn pid_of(stream: Box<dyn Stream>) -> Option<u32> {
    let hello = tokio::time::timeout(bosun_client::CONNECT_TIMEOUT, Client::over(stream));
    Some(hello.await.ok()?.ok()?.daemon().pid)
}

#[cfg(unix)]
pub(crate) use unix::Listener;
#[cfg(windows)]
pub(crate) use windows::Listener;

#[cfg(unix)]
mod unix {
    use std::fs::{DirBuilder, Permissions};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::PathBuf;

    use bosun_client::{Endpoint, Stream};
    use tokio::net::UnixListener;

    /// A bound Unix socket. [`Listener::close`] removes its file.
    #[derive(Debug)]
    pub(crate) struct Listener {
        inner: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        /// Binds the socket: creates its directory (mode 0700) when missing, removes a stale
        /// socket file, and sets the socket's mode to 0600.
        pub(crate) fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let path = endpoint.path().to_owned();
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            }
            // Reached only when nothing answered at the path, so a file there is stale.
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            let inner = UnixListener::bind(&path)?;
            // The directory is private too, which covers the moment between bind and chmod.
            if !cfg!(mutant_api_default_socket_mode) {
                std::fs::set_permissions(&path, Permissions::from_mode(0o600))?;
            }
            Ok(Self { inner, path })
        }

        pub(crate) async fn accept(&mut self) -> io::Result<Box<dyn Stream>> {
            let (stream, _) = self.inner.accept().await?;
            Ok(Box::new(stream))
        }

        /// Stops listening and removes the socket file.
        pub(crate) fn close(self) {
            drop(self.inner);
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::PathBuf;

    use bosun_client::{Endpoint, Stream};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    /// A named pipe server. One instance always waits for the next client.
    #[derive(Debug)]
    pub(crate) struct Listener {
        next: NamedPipeServer,
        path: PathBuf,
    }

    impl Listener {
        /// Creates the first instance of the pipe; fails with `PermissionDenied` when one
        /// exists. Default security (the DACL is stage 1, #4); remote clients are refused.
        pub(crate) fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let path = endpoint.path().to_owned();
            let next = ServerOptions::new()
                .first_pipe_instance(!cfg!(mutant_api_skip_instance_check))
                .reject_remote_clients(true)
                .create(&path)?;
            Ok(Self { next, path })
        }

        pub(crate) async fn accept(&mut self) -> io::Result<Box<dyn Stream>> {
            let connected = self.next.connect().await;
            let following = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.path)?;
            // A failed instance is replaced too: a client that went away breaks it.
            let instance = std::mem::replace(&mut self.next, following);
            connected?;
            Ok(Box::new(instance))
        }

        /// Stops listening; the pipe goes away with its last handle.
        pub(crate) fn close(self) {
            drop(self.next);
        }
    }
}
