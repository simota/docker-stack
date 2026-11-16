//! Turns the Run form's text fields into an [`engine::RunSpec`], the way `docker run`
//! reads its flags. Everything is checked here, before the daemon sees it; host paths
//! are refused, since the UI may not hand them to the daemon.

use std::net::IpAddr;

use engine::{ExecSpec, PublishedPort, RestartPolicy, RunSpec, VolumeMount};

use crate::commands::{invalid, valid_name};
use crate::dto::{ExecShell, RestartChoice, RunForm, UiError};

type R<T> = Result<T, UiError>;

/// The whole form, or the first field that is wrong.
pub fn parse(form: &RunForm) -> R<RunSpec> {
    let image = form.image.trim();
    if image.is_empty() || image.chars().any(char::is_whitespace) {
        return Err(invalid("Choose an image to run."));
    }
    let name = optional(&form.name)
        .map(|n| valid_name(n).map_err(|e| prefixed("Name", e)))
        .transpose()?;
    let network = optional(&form.network)
        .map(|n| valid_name(n).map_err(|e| prefixed("Network", e)))
        .transpose()?;
    Ok(RunSpec {
        image: image.to_owned(),
        name,
        cmd: parse_command(&form.command)?,
        env: parse_env(&form.env)?,
        ports: parse_ports(&form.ports)?,
        volumes: parse_volumes(&form.volumes)?,
        network,
        restart: match form.restart {
            RestartChoice::No => RestartPolicy::No,
            RestartChoice::Always => RestartPolicy::Always,
            RestartChoice::UnlessStopped => RestartPolicy::UnlessStopped,
            RestartChoice::OnFailure => RestartPolicy::OnFailure,
        },
    })
}

/// A login shell: bash if the image has it, else sh.
const DEFAULT_SHELL: &str = "if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi";

/// The Exec tab's fields, or the first one that is wrong.
pub fn parse_exec(shell: &ExecShell) -> R<ExecSpec> {
    let mut cmd = parse_command(&shell.command).map_err(|e| prefixed("Command", e))?;
    if cmd.is_empty() {
        cmd = vec!["sh".into(), "-c".into(), DEFAULT_SHELL.into()];
    }
    let word = |field: &str, s: &str| -> R<Option<String>> {
        match optional(s) {
            Some(v) if v.chars().any(char::is_whitespace) => {
                Err(invalid(format!("{field}: no spaces allowed.")))
            }
            v => Ok(v.map(str::to_owned)),
        }
    };
    Ok(ExecSpec {
        cmd,
        user: word("User", &shell.user)?,
        working_dir: word("Working directory", &shell.working_dir)?,
    })
}

fn optional(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty()).then_some(s)
}

fn prefixed(field: &str, e: UiError) -> UiError {
    invalid(format!("{field}: {}", e.message))
}

/// Entries separated by commas, spaces or new lines.
fn entries(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|e| !e.is_empty())
}

fn port_number(s: &str, entry: &str) -> R<u16> {
    match s.parse::<u16>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(invalid(format!(
            "Ports: {entry}: {s:?} is not a port number (1-65535)."
        ))),
    }
}

/// `-p` entries: `container`, `host:container`, `ip:host:container` or
/// `ip::container` (any free host port), each with an optional `/tcp`, `/udp` or `/sctp`.
/// An IPv6 address goes in brackets: `[::1]:8080:80`.
pub fn parse_ports(s: &str) -> R<Vec<PublishedPort>> {
    entries(s)
        .map(|entry| {
            let (spec, protocol) = match entry.rsplit_once('/') {
                Some((spec, proto)) => (spec, proto.to_ascii_lowercase()),
                None => (entry, "tcp".to_owned()),
            };
            if !matches!(protocol.as_str(), "tcp" | "udp" | "sctp") {
                return Err(invalid(format!(
                    "Ports: {entry}: the protocol must be tcp, udp or sctp."
                )));
            }
            let (ip, rest) = if let Some(after) = spec.strip_prefix('[') {
                let (ip, rest) = after.split_once("]:").ok_or_else(|| {
                    invalid(format!("Ports: {entry}: expected [ip]:host:container."))
                })?;
                (Some(ip), rest)
            } else {
                match spec.matches(':').count() {
                    0 | 1 => (None, spec),
                    2 => {
                        let (ip, rest) = spec.split_once(':').unwrap_or_default();
                        (Some(ip), rest)
                    }
                    _ => {
                        return Err(invalid(format!(
                            "Ports: {entry}: put an IPv6 address in brackets, like [::1]:8080:80."
                        )));
                    }
                }
            };
            let host_ip = ip
                .map(|ip| {
                    ip.parse::<IpAddr>().map(|a| a.to_string()).map_err(|_| {
                        invalid(format!("Ports: {entry}: {ip:?} is not an IP address."))
                    })
                })
                .transpose()?;
            let (host, container) = match rest.split_once(':') {
                Some((host, container)) => (Some(host), container),
                None if host_ip.is_some() => {
                    return Err(invalid(format!(
                        "Ports: {entry}: expected ip:host:container."
                    )));
                }
                None => (None, rest),
            };
            let host_port = host
                .filter(|h| !h.is_empty())
                .map(|h| port_number(h, entry))
                .transpose()?;
            Ok(PublishedPort {
                host_ip,
                host_port,
                container_port: port_number(container, entry)?,
                protocol,
            })
        })
        .collect()
}

/// One `KEY=VALUE` per line; blank lines and `#` comments are skipped. A bare `KEY`
/// (which the CLI copies from its own environment) is refused: the app's environment
/// is not the user's shell.
pub fn parse_env(s: &str) -> R<Vec<String>> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|line| {
            let Some((key, _)) = line.split_once('=') else {
                return Err(invalid(format!(
                    "Environment: {line:?} needs a value, as KEY=VALUE."
                )));
            };
            if key.is_empty() || key.chars().any(char::is_whitespace) {
                return Err(invalid(format!(
                    "Environment: {line:?} needs a name without spaces before '='."
                )));
            }
            Ok(line.to_owned())
        })
        .collect()
}

/// `volume:/path/in/container` entries, with an optional `:ro` (or `:rw`). Only named
/// volumes: a host path is refused. A volume that does not exist yet is created.
pub fn parse_volumes(s: &str) -> R<Vec<VolumeMount>> {
    entries(s)
        .map(|entry| {
            // `/abs`, `./rel`, `~/home`, `\\share` or a Windows drive (`C:\`).
            let drive = entry.as_bytes().get(1..3) == Some(b":\\".as_slice());
            let host_path = entry.starts_with(['/', '.', '~', '\\']) || drive;
            if host_path {
                return Err(invalid(format!(
                    "Volumes: {entry}: host paths cannot be mounted from the app; use a named volume."
                )));
            }
            let mut parts = entry.split(':');
            let (Some(volume), Some(target)) = (parts.next(), parts.next()) else {
                return Err(invalid(format!(
                    "Volumes: {entry}: expected volume:/path/in/container."
                )));
            };
            let read_only = match parts.next() {
                None | Some("rw") => false,
                Some("ro") => true,
                Some(other) => {
                    return Err(invalid(format!(
                        "Volumes: {entry}: {other:?} is not ro or rw."
                    )));
                }
            };
            if parts.next().is_some() {
                return Err(invalid(format!(
                    "Volumes: {entry}: expected volume:/path/in/container[:ro]."
                )));
            }
            let volume = valid_name(volume).map_err(|e| prefixed("Volumes", e))?;
            if !target.starts_with('/') {
                return Err(invalid(format!(
                    "Volumes: {entry}: the path in the container must start with '/'."
                )));
            }
            Ok(VolumeMount {
                volume,
                target: target.to_owned(),
                read_only,
            })
        })
        .collect()
}

/// Splits a command line into words as a POSIX shell does for quotes and backslashes
/// (no variables, globs or pipes: the words go to the container as they are).
pub fn parse_command(s: &str) -> R<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err(invalid("Command: a ' quote is not closed.")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err(invalid("Command: a \" quote is not closed.")),
                        },
                        Some(c) => word.push(c),
                        None => return Err(invalid("Command: a \" quote is not closed.")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(c) = chars.next() {
                    word.push(c);
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// The form that [`parse`] reads back as `spec`: the inverse, for a form filled in from
/// an existing container.
pub fn form_from_spec(spec: &RunSpec) -> RunForm {
    let ports = spec
        .ports
        .iter()
        .map(|p| {
            let container = match p.protocol.as_str() {
                "tcp" => p.container_port.to_string(),
                proto => format!("{}/{proto}", p.container_port),
            };
            let host = p.host_port.map(|n| n.to_string()).unwrap_or_default();
            match &p.host_ip {
                Some(ip) if ip.contains(':') => format!("[{ip}]:{host}:{container}"),
                Some(ip) => format!("{ip}:{host}:{container}"),
                None if host.is_empty() => container,
                None => format!("{host}:{container}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let volumes = spec
        .volumes
        .iter()
        .map(volume_entry)
        .collect::<Vec<_>>()
        .join(", ");
    RunForm {
        image: spec.image.clone(),
        name: spec.name.clone().unwrap_or_default(),
        command: quote_command(&spec.cmd),
        ports,
        env: spec.env.join("\n"),
        volumes,
        network: spec.network.clone().unwrap_or_default(),
        restart: match spec.restart {
            RestartPolicy::No => RestartChoice::No,
            RestartPolicy::Always => RestartChoice::Always,
            RestartPolicy::UnlessStopped => RestartChoice::UnlessStopped,
            RestartPolicy::OnFailure => RestartChoice::OnFailure,
        },
    }
}

/// `volume:/path[:ro]`, as the form's volume box takes it.
fn volume_entry(v: &VolumeMount) -> String {
    let ro = if v.read_only { ":ro" } else { "" };
    format!("{}:{}{ro}", v.volume, v.target)
}

/// Whether `v` survives the form: written into the volume box and read back, it is the
/// same mount (a path with a space or comma is split there).
pub fn volume_round_trips(v: &VolumeMount) -> bool {
    parse_volumes(&volume_entry(v)).is_ok_and(|p| p == [v.clone()])
}

/// Whether `entry` (`KEY=VALUE`) comes back unchanged from a line of the form's
/// environment box: [`parse_env`] reads one entry per line, trims it, and skips
/// comments. A value with a line break or edge spaces would come back different.
pub fn env_round_trips(entry: &str) -> bool {
    !entry.contains(['\n', '\r'])
        && entry.trim() == entry
        && !entry.starts_with('#')
        && parse_env(entry).is_ok_and(|e| e == [entry])
}

/// Joins `words` into a command line that [`parse_command`] splits back into them,
/// single-quoting a word only when it needs it.
pub fn quote_command(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            let plain = !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
            if plain {
                w.clone()
            } else {
                format!("'{}'", w.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(ip: Option<&str>, host: Option<u16>, container: u16, proto: &str) -> PublishedPort {
        PublishedPort {
            host_ip: ip.map(str::to_owned),
            host_port: host,
            container_port: container,
            protocol: proto.into(),
        }
    }

    #[test]
    fn ports_read_every_docker_run_form() {
        let ports = parse_ports(
            "8080:80, 443:443/TCP\n53/udp 127.0.0.1:5432:5432 127.0.0.1::9000 [::1]:6379:6379/tcp",
        )
        .unwrap();
        assert_eq!(
            ports,
            [
                port(None, Some(8080), 80, "tcp"),
                port(None, Some(443), 443, "tcp"),
                port(None, None, 53, "udp"),
                port(Some("127.0.0.1"), Some(5432), 5432, "tcp"),
                port(Some("127.0.0.1"), None, 9000, "tcp"),
                port(Some("::1"), Some(6379), 6379, "tcp"),
            ]
        );
        assert_eq!(parse_ports("  ").unwrap(), []);
    }

    #[test]
    fn ports_refuse_what_docker_would() {
        for bad in [
            "0:80",
            "8080:70000",
            "web:80",
            "80/http",
            "::1:8080:80",
            "localhost:8080:80",
            "[::1]8080:80",
            "1.2.3.4:80",
        ] {
            let e = parse_ports(bad).unwrap_err();
            assert!(e.message.starts_with("Ports:"), "{bad}: {}", e.message);
        }
    }

    #[test]
    fn env_takes_key_value_lines_and_skips_comments() {
        let env = parse_env("A=1\n\n# note\n  URL=http://x/?a=b  \nEMPTY=\n").unwrap();
        assert_eq!(env, ["A=1", "URL=http://x/?a=b", "EMPTY="]);
        assert!(parse_env("HOME").is_err());
        assert!(parse_env("=1").is_err());
        assert!(parse_env("MY KEY=1").is_err());
    }

    #[test]
    fn volumes_mount_named_volumes_and_refuse_host_paths() {
        let v = parse_volumes("data:/var/lib/data, cache:/cache:ro logs:/logs:rw").unwrap();
        assert_eq!(
            v,
            [
                VolumeMount {
                    volume: "data".into(),
                    target: "/var/lib/data".into(),
                    read_only: false
                },
                VolumeMount {
                    volume: "cache".into(),
                    target: "/cache".into(),
                    read_only: true
                },
                VolumeMount {
                    volume: "logs".into(),
                    target: "/logs".into(),
                    read_only: false
                },
            ]
        );
        for host in [
            "/etc:/etc",
            "./src:/src",
            "~/x:/x",
            "C:\\x:/x",
            "\\\\srv\\x:/x",
        ] {
            let e = parse_volumes(host).unwrap_err();
            assert!(e.message.contains("host paths"), "{host}: {}", e.message);
        }
        for bad in [
            "data",
            "data:rel/path",
            "data:/x:rx",
            "data:/x:ro:z",
            "my vol:/x",
        ] {
            assert!(parse_volumes(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn exec_fields_default_to_a_login_shell_and_keep_what_is_typed() {
        let spec = parse_exec(&ExecShell::default()).unwrap();
        assert_eq!(spec.cmd[..2], ["sh", "-c"]);
        assert!(spec.cmd[2].contains("exec bash"));
        assert_eq!((spec.user, spec.working_dir), (None, None));

        let spec = parse_exec(&ExecShell {
            command: "python3 -i".into(),
            user: " root ".into(),
            working_dir: "/srv/app".into(),
        })
        .unwrap();
        assert_eq!(spec.cmd, ["python3", "-i"]);
        assert_eq!(spec.user.as_deref(), Some("root"));
        assert_eq!(spec.working_dir.as_deref(), Some("/srv/app"));

        let bad = |f: fn(&mut ExecShell)| {
            let mut shell = ExecShell::default();
            f(&mut shell);
            parse_exec(&shell).unwrap_err().message
        };
        assert!(bad(|s| s.command = "sh -c 'open".into()).starts_with("Command: "));
        assert!(bad(|s| s.user = "a b".into()).starts_with("User: "));
        assert!(bad(|s| s.working_dir = "/a b".into()).starts_with("Working directory: "));
    }

    #[test]
    fn command_splits_like_a_shell_without_expanding() {
        assert_eq!(
            parse_command(r#"sh -c 'echo "$HOME" && sleep 5'"#).unwrap(),
            ["sh", "-c", r#"echo "$HOME" && sleep 5"#]
        );
        assert_eq!(
            parse_command(r#"echo "a \"b\" \n" c\ d '' x"#).unwrap(),
            ["echo", r#"a "b" \n"#, "c d", "", "x"]
        );
        assert_eq!(parse_command("   ").unwrap(), Vec::<String>::new());
        assert!(parse_command("echo 'open").is_err());
        assert!(parse_command("echo \"open").is_err());
    }

    #[test]
    fn form_names_the_field_that_is_wrong() {
        let form = |f: fn(&mut RunForm)| {
            let mut form = RunForm {
                image: "nginx:1.27".into(),
                ..Default::default()
            };
            f(&mut form);
            parse(&form)
        };
        let spec = form(|f| {
            f.name = " web ".into();
            f.network = "backend".into();
            f.restart = RestartChoice::UnlessStopped;
            f.ports = "8080:80".into();
        })
        .unwrap();
        assert_eq!(spec.image, "nginx:1.27");
        assert_eq!(spec.name.as_deref(), Some("web"));
        assert_eq!(spec.network.as_deref(), Some("backend"));
        assert_eq!(spec.restart, RestartPolicy::UnlessStopped);
        assert_eq!(spec.ports.len(), 1);

        let plain = form(|_| {}).unwrap();
        assert_eq!((plain.name, plain.network), (None, None));
        assert_eq!(plain.restart, RestartPolicy::No);

        assert!(form(|f| f.image = " ".into()).is_err());
        assert!(
            form(|f| f.name = "-x".into())
                .unwrap_err()
                .message
                .starts_with("Name:")
        );
        assert!(
            form(|f| f.network = "a/b".into())
                .unwrap_err()
                .message
                .starts_with("Network:")
        );
    }

    #[test]
    fn env_entries_that_a_line_cannot_hold_are_told_apart() {
        for ok in ["A=1", "URL=postgres://db/x?a=b", "EMPTY=", "SPACED=a b"] {
            assert!(env_round_trips(ok), "{ok}");
        }
        for lossy in [
            "MESSAGE=first\nSECOND=second",
            "CERT=-----BEGIN\r\nabc",
            "PAD=x ",
            " LEAD=x",
            "#NOT=a comment",
            "BARE",
            "=nokey",
        ] {
            assert!(!env_round_trips(lossy), "{lossy:?}");
        }
    }

    #[test]
    fn volumes_that_the_volume_box_would_split_are_told_apart() {
        let mount = |volume: &str, target: &str, read_only| VolumeMount {
            volume: volume.into(),
            target: target.into(),
            read_only,
        };
        for ok in [
            mount("data", "/srv/data", false),
            mount("data", "/srv/data", true),
        ] {
            assert!(volume_round_trips(&ok), "{ok:?}");
        }
        for split in [
            mount("data", "/srv/my data", false),
            mount("data", "/srv/a,b", true),
            mount("data", "/srv/a:b", false),
        ] {
            assert!(!volume_round_trips(&split), "{split:?}");
        }
    }

    #[test]
    fn quoted_commands_split_back_into_the_same_words() {
        let cases: [&[&str]; 5] = [
            &["nginx", "-g", "daemon off;"],
            &["sh", "-c", "echo 'hi' && echo \"$HOME\""],
            &["printf", "", "a\\b", "tab\there"],
            &["--opt=a,b", "user@host:/path", "50%"],
            &[],
        ];
        for words in cases {
            let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            let line = quote_command(&words);
            assert_eq!(parse_command(&line).unwrap(), words, "{line}");
        }
        assert_eq!(
            quote_command(&["nginx".into(), "-g".into(), "daemon off;".into()]),
            "nginx -g 'daemon off;'"
        );
    }

    #[test]
    fn a_form_from_a_spec_parses_back_to_the_same_spec() {
        let spec = RunSpec {
            image: "registry.local:5000/team/app:1.2".into(),
            name: Some("app-copy".into()),
            cmd: vec!["serve".into(), "--motd".into(), "it's up".into()],
            env: vec![
                "A=1".into(),
                "URL=postgres://db/x?a=b".into(),
                "EMPTY=".into(),
            ],
            ports: vec![
                port(None, None, 9000, "tcp"),
                port(None, Some(8080), 80, "tcp"),
                port(Some("127.0.0.1"), None, 53, "udp"),
                port(Some("::1"), Some(6379), 6379, "tcp"),
                port(Some("0.0.0.0"), Some(5000), 5000, "sctp"),
            ],
            volumes: vec![
                VolumeMount {
                    volume: "data".into(),
                    target: "/var/lib/data".into(),
                    read_only: false,
                },
                VolumeMount {
                    volume: "cache".into(),
                    target: "/cache".into(),
                    read_only: true,
                },
            ],
            network: Some("shop_default".into()),
            restart: RestartPolicy::UnlessStopped,
        };
        let form = form_from_spec(&spec);
        assert_eq!(
            form.ports,
            "9000, 8080:80, 127.0.0.1::53/udp, [::1]:6379:6379, 0.0.0.0:5000:5000/sctp"
        );
        assert_eq!(form.volumes, "data:/var/lib/data, cache:/cache:ro");
        assert_eq!(form.restart, RestartChoice::UnlessStopped);
        assert_eq!(parse(&form).unwrap(), spec);

        let bare = RunSpec {
            image: "alpine".into(),
            ..Default::default()
        };
        let form = form_from_spec(&bare);
        assert_eq!((form.name.as_str(), form.command.as_str()), ("", ""));
        assert_eq!(parse(&form).unwrap(), bare);
    }
}
