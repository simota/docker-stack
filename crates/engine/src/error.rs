use std::fmt;
use std::path::PathBuf;

use crate::ConnectTarget;

/// Errors split only where the UI reacts differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// The socket file does not exist: dockerd is not running or the path is wrong.
    SocketMissing(PathBuf),
    /// The socket exists but this user may not open it (not in the `docker` group).
    PermissionDenied(PathBuf),
    NotFound(String),
    /// The request conflicts with the current state (e.g. removing a running container).
    Conflict(String),
    Daemon {
        status: u16,
        message: String,
    },
    /// The connection broke or could not be made; worth reconnecting.
    Transport(String),
    /// One request took too long; the connection may well be fine.
    Timeout(String),
    /// An Update failed and the original container could not be put back as it was:
    /// it is kept as `kept_as`, and `stopped` when it ran before.
    NotRestored {
        error: Box<EngineError>,
        kept_as: String,
        stopped: bool,
    },
    /// A local failure outside the daemon (e.g. spawning `docker compose`).
    Local(String),
}

impl EngineError {
    pub(crate) fn classify(e: bollard::errors::Error, target: &ConnectTarget) -> Self {
        use bollard::errors::Error as B;
        match e {
            B::SocketNotFoundError(p) => EngineError::SocketMissing(PathBuf::from(p)),
            B::DockerResponseServerError {
                status_code,
                message,
            } => match status_code {
                404 => EngineError::NotFound(message),
                409 => EngineError::Conflict(message),
                s => EngineError::Daemon { status: s, message },
            },
            // The daemon accepted the request and reported a failure part-way through a
            // streamed answer (a pull that runs out of disk, a layer that fails to
            // download): the connection is fine. Same status as `pull`'s `error_detail`.
            B::DockerStreamError { error } => EngineError::Daemon {
                status: 500,
                message: error,
            },
            B::IOError { err } if err.kind() == std::io::ErrorKind::PermissionDenied => {
                EngineError::PermissionDenied(target.socket_path().unwrap_or_default())
            }
            other => {
                let text = other.to_string();
                if text.contains("Permission denied") || text.contains("permission denied") {
                    EngineError::PermissionDenied(target.socket_path().unwrap_or_default())
                } else {
                    EngineError::Transport(text)
                }
            }
        }
    }

    /// Whether the connection itself is gone (as opposed to one request failing).
    pub fn is_connection_lost(&self) -> bool {
        matches!(
            self,
            EngineError::Transport(_)
                | EngineError::SocketMissing(_)
                | EngineError::PermissionDenied(_)
        )
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::SocketMissing(p) => {
                write!(
                    f,
                    "Docker socket not found at {} — is dockerd running?",
                    p.display()
                )
            }
            EngineError::PermissionDenied(p) => write!(
                f,
                "Permission denied on {}. Add your user to the docker group (sudo usermod -aG docker $USER) and log in again.",
                p.display()
            ),
            EngineError::NotFound(m) => write!(f, "Not found: {m}"),
            EngineError::Conflict(m) => write!(f, "Conflict: {m}"),
            EngineError::Daemon { status, message } => {
                write!(f, "Docker error ({status}): {message}")
            }
            EngineError::Transport(m) => write!(f, "Connection error: {m}"),
            EngineError::Timeout(m) => write!(f, "{m}"),
            EngineError::NotRestored {
                error,
                kept_as,
                stopped,
            } => {
                write!(f, "{error}; the original container is kept as {kept_as}")?;
                if *stopped {
                    write!(f, " and stopped")?;
                }
                Ok(())
            }
            EngineError::Local(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for EngineError {}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use bollard::errors::Error as B;

    fn target(host: &str) -> ConnectTarget {
        ConnectTarget {
            host: host.into(),
            source: String::new(),
            context: None,
            tls: None,
        }
    }

    fn server(status_code: u16) -> B {
        B::DockerResponseServerError {
            status_code,
            message: "msg".into(),
        }
    }

    #[test]
    fn classify_splits_errors_where_the_ui_reacts_differently() {
        let t = target("unix:///run/docker.sock");
        let socket = PathBuf::from("/run/docker.sock");
        assert_eq!(
            EngineError::classify(B::SocketNotFoundError("/run/docker.sock".into()), &t),
            EngineError::SocketMissing(socket.clone())
        );
        assert_eq!(
            EngineError::classify(server(404), &t),
            EngineError::NotFound("msg".into())
        );
        assert_eq!(
            EngineError::classify(server(409), &t),
            EngineError::Conflict("msg".into())
        );
        assert_eq!(
            EngineError::classify(server(500), &t),
            EngineError::Daemon {
                status: 500,
                message: "msg".into()
            }
        );
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(
            EngineError::classify(B::IOError { err: denied }, &t),
            EngineError::PermissionDenied(socket.clone())
        );
        // bollard sometimes wraps the OS error in text only.
        let wrapped = io::Error::other("connect: Permission denied (os error 13)");
        assert_eq!(
            EngineError::classify(B::IOError { err: wrapped }, &t),
            EngineError::PermissionDenied(socket)
        );
        // A failure the daemon reports inside a streamed answer, even one that mentions
        // a permission, is that request failing, not the socket.
        for error in [
            "no space left on device",
            "open /var/lib/x: permission denied",
        ] {
            assert_eq!(
                EngineError::classify(
                    B::DockerStreamError {
                        error: error.into()
                    },
                    &t
                ),
                EngineError::Daemon {
                    status: 500,
                    message: error.into()
                }
            );
        }
        let reset = io::Error::from(io::ErrorKind::ConnectionReset);
        assert!(matches!(
            EngineError::classify(B::IOError { err: reset }, &t),
            EngineError::Transport(_)
        ));
        assert!(matches!(
            EngineError::classify(B::RequestTimeoutError, &t),
            EngineError::Transport(_)
        ));
    }

    #[test]
    fn permission_denied_off_a_unix_socket_names_no_path() {
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(
            EngineError::classify(B::IOError { err: denied }, &target("tcp://h:2376")),
            EngineError::PermissionDenied(PathBuf::new())
        );
    }

    #[test]
    fn only_a_lost_connection_reconnects() {
        let lost = [
            EngineError::Transport(String::new()),
            EngineError::SocketMissing(PathBuf::new()),
            EngineError::PermissionDenied(PathBuf::new()),
        ];
        assert!(lost.iter().all(EngineError::is_connection_lost));
        let request = [
            EngineError::NotFound(String::new()),
            EngineError::Conflict(String::new()),
            EngineError::Daemon {
                status: 500,
                message: String::new(),
            },
            EngineError::Local(String::new()),
            EngineError::Timeout(String::new()),
            EngineError::NotRestored {
                error: Box::new(EngineError::Transport(String::new())),
                kept_as: String::new(),
                stopped: true,
            },
        ];
        assert!(!request.iter().any(EngineError::is_connection_lost));
    }
}
