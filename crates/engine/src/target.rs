//! Where to connect: `DOCKER_HOST`, the current Docker context, the rootless
//! socket, then `/var/run/docker.sock`. Docker's own config is only read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::EngineError;

pub(crate) const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
std::thread_local! {
    static META_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn read_meta_file(path: &Path) -> Option<String> {
    #[cfg(test)]
    META_READS.with(|c| c.set(c.get() + 1));
    std::fs::read_to_string(path).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsFiles {
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTarget {
    /// A Docker host URL such as `unix:///var/run/docker.sock` or `tcp://host:2375`.
    pub host: String,
    /// Where this target came from, for display ("DOCKER_HOST", "context: foo", ...).
    pub source: String,
    /// The Docker context name, when the target came from one.
    pub context: Option<String>,
    /// Client/CA certificates when TLS verification is enabled for this target.
    pub tls: Option<TlsFiles>,
}

impl ConnectTarget {
    pub fn is_supported_scheme(host: &str) -> bool {
        !host.starts_with("ssh://")
    }

    pub fn socket_path(&self) -> Option<PathBuf> {
        self.host.strip_prefix("unix://").map(PathBuf::from)
    }

    /// Distinguishes a missing socket from a socket this user may not open,
    /// before handing the host to bollard (whose errors blur the two).
    pub(crate) async fn preflight(&self) -> Result<(), EngineError> {
        self.preflight_with_timeout(PREFLIGHT_TIMEOUT).await
    }

    pub(crate) async fn preflight_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(), EngineError> {
        if !Self::is_supported_scheme(&self.host) {
            return Err(EngineError::Local(format!(
                "SSH Docker endpoint {} is not supported; use a unix:// or tcp:// context.",
                self.host
            )));
        }
        let Some(path) = self.socket_path() else {
            return Ok(());
        };
        match tokio::time::timeout(timeout, tokio::net::UnixStream::connect(&path)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                Err(EngineError::PermissionDenied(path))
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(EngineError::SocketMissing(path))
            }
            Ok(Err(e)) => Err(EngineError::Transport(format!("{}: {e}", path.display()))),
            Err(_) => Err(EngineError::Transport(format!(
                "{}: connection timed out",
                path.display()
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEntry {
    pub name: String,
    pub host: String,
    pub current: bool,
    pub supported: bool,
    pub tls: Option<TlsFiles>,
}

pub(crate) struct TargetEnv<'a> {
    pub docker_host: Option<&'a str>,
    pub docker_context: Option<&'a str>,
    pub docker_tls_verify: bool,
    pub docker_cert_path: Option<&'a Path>,
    pub config_dir: Option<&'a Path>,
    pub xdg_runtime_dir: Option<&'a Path>,
    pub system_socket: &'a Path,
}

fn env_docker_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DOCKER_CONFIG") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".docker"))
}

fn tls_files_in(dir: &Path) -> Option<TlsFiles> {
    let ca = dir.join("ca.pem");
    ca.is_file().then(|| TlsFiles {
        ca,
        cert: dir.join("cert.pem"),
        key: dir.join("key.pem"),
    })
}

fn current_context_name(config_dir: &Path, env_context: Option<&str>) -> Option<String> {
    if let Some(name) = env_context.filter(|s| !s.is_empty()) {
        return Some(name.to_owned());
    }
    let raw = std::fs::read_to_string(config_dir.join("config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("currentContext")?.as_str().map(str::to_owned)
}

fn context_meta(config_dir: &Path, name: &str) -> Option<(String, Option<TlsFiles>)> {
    let digest = Sha256::digest(name.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let meta = config_dir
        .join("contexts/meta")
        .join(&hex)
        .join("meta.json");
    let raw = read_meta_file(&meta)?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if let Some(stored_name) = v.get("Name").and_then(|n| n.as_str())
        && stored_name != name
    {
        return None;
    }
    let host = v.pointer("/Endpoints/docker/Host")?.as_str()?.to_owned();
    let skip_tls = v
        .pointer("/Endpoints/docker/SkipTLSVerify")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let tls = (!skip_tls)
        .then(|| config_dir.join("contexts/tls").join(&hex).join("docker"))
        .and_then(|d| tls_files_in(&d));
    Some((host, tls))
}

fn rootless_socket_in(xdg_runtime_dir: Option<&Path>) -> Option<String> {
    let p = xdg_runtime_dir?.join("docker.sock");
    p.exists().then(|| format!("unix://{}", p.display()))
}

const SYSTEM_SOCKET_PATH: &str = "/var/run/docker.sock";

/// The context this crate adds for the rootless Docker socket (`$XDG_RUNTIME_DIR/docker.sock`).
/// The docker CLI does not know it: it is listed and found only here.
pub const ROOTLESS_CONTEXT: &str = "rootless (local)";

pub(crate) fn resolve_default_target_with(env: &TargetEnv<'_>) -> ConnectTarget {
    if let Some(host) = env.docker_host.filter(|h| !h.is_empty()) {
        let tls = if env.docker_tls_verify {
            env.docker_cert_path
                .or(env.config_dir)
                .and_then(tls_files_in)
        } else {
            None
        };
        return ConnectTarget {
            host: host.to_owned(),
            source: "DOCKER_HOST".into(),
            context: None,
            tls,
        };
    }
    if let Some(dir) = env.config_dir
        && let Some(name) = current_context_name(dir, env.docker_context).filter(|n| n != "default")
        && let Some((host, tls)) = context_meta(dir, &name)
    {
        return ConnectTarget {
            host,
            source: format!("context: {name}"),
            context: Some(name),
            tls,
        };
    }
    if !env.system_socket.exists()
        && let Some(host) = rootless_socket_in(env.xdg_runtime_dir)
    {
        return ConnectTarget {
            host,
            source: "rootless".into(),
            context: None,
            tls: None,
        };
    }
    ConnectTarget {
        host: format!("unix://{}", env.system_socket.display()),
        source: "default".into(),
        context: Some("default".into()),
        tls: None,
    }
}

pub(crate) fn list_contexts_with(env: &TargetEnv<'_>) -> Vec<ContextEntry> {
    let host_override = env.docker_host.is_some_and(|h| !h.is_empty());
    let current = env
        .config_dir
        .and_then(|d| current_context_name(d, env.docker_context))
        .unwrap_or_else(|| "default".into());
    let default_host = format!("unix://{}", env.system_socket.display());
    let mut out = vec![ContextEntry {
        name: "default".into(),
        host: default_host,
        current: !host_override && current == "default",
        supported: true,
        tls: None,
    }];
    if let Some(host) = rootless_socket_in(env.xdg_runtime_dir) {
        out.push(ContextEntry {
            name: ROOTLESS_CONTEXT.into(),
            host,
            current: false,
            supported: true,
            tls: None,
        });
    }
    let Some(dir) = env.config_dir else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(dir.join("contexts/meta")) else {
        return out;
    };
    let mut named: Vec<ContextEntry> = entries
        .flatten()
        .filter_map(|e| {
            let meta_dir = e.path();
            let raw = read_meta_file(&meta_dir.join("meta.json"))?;
            let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
            let name = v.get("Name")?.as_str()?.to_owned();
            let host = v.pointer("/Endpoints/docker/Host")?.as_str()?.to_owned();
            let skip_tls = v
                .pointer("/Endpoints/docker/SkipTLSVerify")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let hex = meta_dir.file_name()?.to_str()?;
            let tls = (!skip_tls)
                .then(|| dir.join("contexts/tls").join(hex).join("docker"))
                .and_then(|d| tls_files_in(&d));
            Some(ContextEntry {
                current: !host_override && name == current,
                supported: ConnectTarget::is_supported_scheme(&host),
                name,
                host,
                tls,
            })
        })
        .collect();
    named.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(named);
    out
}

pub(crate) fn find_context_with(env: &TargetEnv<'_>, name: &str) -> Option<ContextEntry> {
    let host_override = env.docker_host.is_some_and(|h| !h.is_empty());
    let current = env
        .config_dir
        .and_then(|d| current_context_name(d, env.docker_context))
        .unwrap_or_else(|| "default".into());
    if name == "default" {
        return Some(ContextEntry {
            name: "default".into(),
            host: format!("unix://{}", env.system_socket.display()),
            current: !host_override && current == "default",
            supported: true,
            tls: None,
        });
    }
    if name == ROOTLESS_CONTEXT {
        let host = rootless_socket_in(env.xdg_runtime_dir)?;
        return Some(ContextEntry {
            name: ROOTLESS_CONTEXT.into(),
            host,
            current: false,
            supported: true,
            tls: None,
        });
    }
    let dir = env.config_dir?;
    let (host, tls) = context_meta(dir, name)?;
    Some(ContextEntry {
        name: name.to_owned(),
        current: !host_override && name == current,
        supported: ConnectTarget::is_supported_scheme(&host),
        host,
        tls,
    })
}

fn with_process_env<R>(f: impl FnOnce(&TargetEnv<'_>) -> R) -> R {
    let docker_host = std::env::var("DOCKER_HOST").ok();
    let docker_context = std::env::var("DOCKER_CONTEXT").ok();
    let docker_tls_verify = std::env::var("DOCKER_TLS_VERIFY")
        .ok()
        .is_some_and(|v| !v.is_empty() && v != "0");
    let docker_cert_path = std::env::var_os("DOCKER_CERT_PATH").map(PathBuf::from);
    let config_dir = env_docker_config_dir();
    let xdg_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let env = TargetEnv {
        docker_host: docker_host.as_deref(),
        docker_context: docker_context.as_deref(),
        docker_tls_verify,
        docker_cert_path: docker_cert_path.as_deref(),
        config_dir: config_dir.as_deref(),
        xdg_runtime_dir: xdg_runtime_dir.as_deref(),
        system_socket: Path::new(SYSTEM_SOCKET_PATH),
    };
    f(&env)
}

/// The target the `docker` CLI would use with no flags.
pub fn resolve_default_target() -> ConnectTarget {
    with_process_env(resolve_default_target_with)
}

/// The `default` context plus every context stored under `~/.docker/contexts`.
pub fn list_contexts() -> Vec<ContextEntry> {
    with_process_env(list_contexts_with)
}

/// Resolves a single named context in O(1) via Docker's SHA-256 metadata layout.
pub fn find_context(name: &str) -> Option<ContextEntry> {
    with_process_env(|env| find_context_with(env, name))
}

impl From<&ContextEntry> for ConnectTarget {
    fn from(c: &ContextEntry) -> Self {
        ConnectTarget {
            host: c.host.clone(),
            source: format!("context: {}", c.name),
            context: Some(c.name.clone()),
            tls: c.tls.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_context(root: &Path, name: &str, host: &str, skip_tls: bool, with_ca: bool) {
        let digest: String = Sha256::digest(name.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let meta_dir = root.join("contexts/meta").join(&digest);
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::write(
            meta_dir.join("meta.json"),
            format!(
                r#"{{"Name":"{name}","Endpoints":{{"docker":{{"Host":"{host}","SkipTLSVerify":{skip_tls}}}}}}}"#
            ),
        )
        .unwrap();
        if with_ca {
            let tls_dir = root.join("contexts/tls").join(&digest).join("docker");
            std::fs::create_dir_all(&tls_dir).unwrap();
            std::fs::write(tls_dir.join("ca.pem"), b"cert").unwrap();
        }
    }

    #[test]
    fn context_host_reads_meta_by_sha256_of_name() {
        let tmp = std::env::temp_dir().join(format!("ds-ctx-{}", std::process::id()));
        write_context(&tmp, "remote", "tcp://10.0.0.1:2375", false, false);
        assert_eq!(
            context_meta(&tmp, "remote").map(|(h, _)| h).as_deref(),
            Some("tcp://10.0.0.1:2375")
        );
        assert_eq!(context_meta(&tmp, "missing"), None);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn context_tls_and_ssh_support_are_classified() {
        // A-003 / R-03
        let tmp = std::env::temp_dir().join(format!("ds-ctx-tls-{}", std::process::id()));
        write_context(&tmp, "secure", "tcp://10.0.0.2:2376", false, true);
        write_context(&tmp, "sshbox", "ssh://ops@10.0.0.3", false, false);
        let env = TargetEnv {
            docker_host: None,
            docker_context: Some("secure"),
            docker_tls_verify: false,
            docker_cert_path: None,
            config_dir: Some(&tmp),
            xdg_runtime_dir: None,
            system_socket: Path::new("/nonexistent/docker.sock"),
        };
        let resolved = resolve_default_target_with(&env);
        assert_eq!(resolved.context.as_deref(), Some("secure"));
        assert!(
            resolved
                .tls
                .as_ref()
                .is_some_and(|t| t.ca.ends_with("ca.pem"))
        );

        let entries = list_contexts_with(&env);
        let secure = entries.iter().find(|c| c.name == "secure").unwrap();
        assert!(secure.current);
        assert!(secure.supported);
        assert!(secure.tls.is_some());

        let ssh = entries.iter().find(|c| c.name == "sshbox").unwrap();
        assert!(!ssh.supported);
        let ssh_target = ConnectTarget::from(ssh);
        assert!(matches!(
            futures::executor::block_on(crate::runtime::spawn(async move {
                ssh_target.preflight().await
            })),
            Err(EngineError::Local(_))
        ));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn precedence_orders_docker_host_context_rootless_and_default() {
        // A-006, A-009 / R-03, R-07
        let tmp = std::env::temp_dir().join(format!("ds-prec-{}", std::process::id()));
        let xdg = tmp.join("run");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::write(xdg.join("docker.sock"), b"").unwrap();
        write_context(&tmp, "ctx1", "tcp://127.0.0.1:2375", false, false);
        let missing_sys = tmp.join("no-system.sock");

        let by_host = TargetEnv {
            docker_host: Some("unix:///var/run/docker.sock"),
            docker_context: Some("ctx1"),
            docker_tls_verify: false,
            docker_cert_path: None,
            config_dir: Some(&tmp),
            xdg_runtime_dir: Some(&xdg),
            system_socket: &missing_sys,
        };
        let t = resolve_default_target_with(&by_host);
        assert_eq!(t.source, "DOCKER_HOST");
        assert_eq!(t.context, None);
        assert!(list_contexts_with(&by_host).iter().all(|c| !c.current));

        let by_ctx = TargetEnv {
            docker_host: None,
            ..by_host
        };
        assert_eq!(
            resolve_default_target_with(&by_ctx).context.as_deref(),
            Some("ctx1")
        );

        let by_rootless = TargetEnv {
            docker_host: None,
            docker_context: None,
            ..by_host
        };
        assert_eq!(resolve_default_target_with(&by_rootless).source, "rootless");

        std::fs::remove_file(xdg.join("docker.sock")).unwrap();
        assert_eq!(
            resolve_default_target_with(&by_rootless).context.as_deref(),
            Some("default")
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn socket_path_only_for_unix_hosts() {
        let t = ConnectTarget {
            host: "unix:///run/x.sock".into(),
            source: String::new(),
            context: None,
            tls: None,
        };
        assert_eq!(t.socket_path(), Some(PathBuf::from("/run/x.sock")));
        let t = ConnectTarget {
            host: "tcp://h:1".into(),
            source: String::new(),
            context: None,
            tls: None,
        };
        assert_eq!(t.socket_path(), None);
    }

    #[test]
    fn preflight_reports_missing_socket() {
        let t = ConnectTarget {
            host: "unix:///nonexistent/docker.sock".into(),
            source: String::new(),
            context: None,
            tls: None,
        };
        assert_eq!(
            futures::executor::block_on(crate::runtime::spawn(async move { t.preflight().await })),
            Err(EngineError::SocketMissing(
                "/nonexistent/docker.sock".into()
            ))
        );
    }

    #[test]
    fn find_context_resolves_by_sha256_in_constant_reads() {
        // P-011: direct context lookup reads one meta.json instead of scanning all K contexts.
        let tmp = std::env::temp_dir().join(format!("ds-find-ctx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for i in 0..30 {
            write_context(
                &tmp,
                &format!("ctx-{i:02}"),
                &format!("tcp://10.0.0.{i}:2375"),
                false,
                i == 17,
            );
        }
        let env = TargetEnv {
            docker_host: None,
            docker_context: Some("ctx-17"),
            docker_tls_verify: false,
            docker_cert_path: None,
            config_dir: Some(&tmp),
            xdg_runtime_dir: None,
            system_socket: Path::new("/nonexistent/docker.sock"),
        };

        // Positive control: full enumeration reads all 30 meta.json files.
        META_READS.with(|c| c.set(0));
        let all = list_contexts_with(&env);
        let full_reads = META_READS.with(|c| c.get());
        assert_eq!(full_reads, 30);
        let expected = all.into_iter().find(|c| c.name == "ctx-17").unwrap();

        // Fixed path: direct lookup reads only 1 meta.json file and returns the same entry.
        META_READS.with(|c| c.set(0));
        let found = find_context_with(&env, "ctx-17").unwrap();
        let direct_reads = META_READS.with(|c| c.get());
        assert_eq!(direct_reads, 1);
        assert_eq!(found, expected);
        assert_eq!(find_context_with(&env, "default").unwrap().name, "default");
        assert_eq!(find_context_with(&env, "missing"), None);

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn preflight_connects_async_and_times_out_or_errors_without_blocking() {
        // P-012: preflight uses non-blocking tokio UnixStream::connect with a bounded timeout.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let sock = std::env::temp_dir().join(format!("ds-preflight-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);

        rt.block_on(async {
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let ok_target = ConnectTarget {
                host: format!("unix://{}", sock.display()),
                source: String::new(),
                context: None,
                tls: None,
            };
            // Positive control: active UnixListener succeeds asynchronously.
            assert_eq!(
                ok_target
                    .preflight_with_timeout(Duration::from_millis(200))
                    .await,
                Ok(())
            );
            drop(listener);
            std::fs::remove_file(&sock).unwrap();
            std::fs::write(&sock, b"not-a-socket").unwrap();
            let res = ok_target
                .preflight_with_timeout(Duration::from_millis(50))
                .await;
            assert!(matches!(res, Err(EngineError::Transport(_))), "{res:?}");
        });
        let _ = std::fs::remove_file(&sock);
    }
}
