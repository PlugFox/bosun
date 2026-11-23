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
    use bosun_proc::pipe::OwnerOnly;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    /// A named pipe server. One instance always waits for the next client.
    #[derive(Debug)]
    pub(crate) struct Listener {
        next: NamedPipeServer,
        path: PathBuf,
        owner: OwnerOnly,
    }

    impl Listener {
        /// Creates the first instance of the pipe; fails with `PermissionDenied` when one
        /// exists. Every instance has a DACL with one entry, for the user the daemon runs as
        /// (#4, #71); remote clients are refused.
        pub(crate) fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let path = endpoint.path().to_owned();
            let owner = OwnerOnly::new()?;
            let mut options = ServerOptions::new();
            options
                .first_pipe_instance(!cfg!(mutant_api_skip_instance_check))
                .reject_remote_clients(true);
            let next = create(&owner, &options, &path)?;
            Ok(Self { next, path, owner })
        }

        pub(crate) async fn accept(&mut self) -> io::Result<Box<dyn Stream>> {
            let connected = self.next.connect().await;
            let mut options = ServerOptions::new();
            options.reject_remote_clients(true);
            let following = create(&self.owner, &options, &self.path)?;
            // A failed instance is replaced too: a client that went away breaks it.
            let instance = std::mem::replace(&mut self.next, following);
            connected?;
            Ok(Box::new(instance))
        }

        /// Stops listening; the pipe goes away with its last handle.
        pub(crate) fn close(self) {
            drop(self.next);
        }

        /// The instance waiting for the next client.
        #[cfg(test)]
        pub(crate) fn next(&self) -> &NamedPipeServer {
            &self.next
        }
    }

    /// One instance of the pipe, with the owner-only descriptor, read back before it is used:
    /// an instance whose DACL admits anyone else is refused.
    fn create(
        owner: &OwnerOnly,
        options: &ServerOptions,
        path: &PathBuf,
    ) -> io::Result<NamedPipeServer> {
        // Mutant `mutant_api_default_pipe_acl` (#71): the default descriptor, which admits
        // SYSTEM and the Administrators too.
        let pipe = if cfg!(mutant_api_default_pipe_acl) {
            options.create(path)?
        } else {
            owner.create_pipe(options, path)?
        };
        owner.verify(&pipe)?;
        Ok(pipe)
    }

    #[cfg(test)]
    mod tests {
        use bosun_proc::pipe::{AceKind, current_user_sid, dacl};

        use super::*;

        #[tokio::test]
        async fn every_instance_admits_only_the_owner() {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos();
            let endpoint = Endpoint::new(format!("bosund-acl-test-{}-{nanos}", std::process::id()));
            let mut listener = Listener::bind(&endpoint).unwrap();
            let user = current_user_sid().unwrap();
            let only_owner = |pipe: &NamedPipeServer| {
                let aces = dacl(pipe).unwrap();
                assert_eq!(aces.len(), 1, "{aces:?}");
                assert_eq!(aces[0].kind, AceKind::Allow, "{aces:?}");
                assert_eq!(aces[0].sid, user, "{aces:?}");
            };
            only_owner(listener.next());
            // The instance created for the next client after one connects.
            let client = tokio::net::windows::named_pipe::ClientOptions::new()
                .open(endpoint.path())
                .unwrap();
            let accepted = listener.accept().await.unwrap();
            only_owner(listener.next());
            drop((client, accepted));
            listener.close();
        }
    }
}
