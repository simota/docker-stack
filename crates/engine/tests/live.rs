//! Tests against the local dockerd. Run with `DOCKER_STACK_LIVE=1 cargo test -p engine --test live`.
//! They are awaited with `futures::executor::block_on` on purpose: the UI's executor
//! is not tokio, so this proves results cross the runtime boundary.

use std::time::Duration;

use engine::*;
use futures::StreamExt;
use futures::executor::block_on;

fn live() -> Option<Engine> {
    if std::env::var("DOCKER_STACK_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set DOCKER_STACK_LIVE=1");
        return None;
    }
    let (engine, info) = block_on(Engine::connect(resolve_default_target())).expect("connect");
    assert!(!info.version.is_empty());
    Some(engine)
}

fn ensure_image(engine: &Engine, reference: &str) {
    let mut rx = engine.pull(reference);
    block_on(async {
        while let Some(p) = rx.next().await {
            p.expect("pull");
        }
    });
}

#[test]
fn lists_from_foreign_executor() {
    let Some(engine) = live() else { return };
    block_on(engine.containers(true)).expect("containers");
    block_on(engine.images()).expect("images");
    block_on(engine.volumes()).expect("volumes");
    let nets = block_on(engine.networks()).expect("networks");
    assert!(nets.iter().any(|n| n.name == "bridge"));

    let cap = block_on(engine.system_capacity()).expect("system_capacity");
    assert!(cap.cpu_cores >= 1);
    assert!(cap.mem_total > 0);
    assert!(cap.disk_total.is_some_and(|n| n > 0));

    let disk = block_on(engine.disk_usage()).expect("disk_usage");
    assert!(disk.is_some());
}

#[test]
fn container_lifecycle_logs_events_exec() {
    let Some(engine) = live() else { return };
    ensure_image(&engine, "alpine:3.20");
    let name = format!("ds-live-{}", std::process::id());
    let status = std::process::Command::new("docker")
        .args([
            "create",
            "--name",
            &name,
            "alpine:3.20",
            "sh",
            "-c",
            "echo hello-out; echo hello-err >&2; sleep 300",
        ])
        .output()
        .expect("docker create");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );

    let mut events = engine.events();
    block_on(engine.container_action(&name, ContainerAction::Start)).expect("start");

    let list = block_on(engine.containers(false)).unwrap();
    let c = list.iter().find(|c| c.name == name).expect("listed");
    assert_eq!(c.state, ContainerState::Running);

    // events: a start event for this container arrives.
    let got_start = block_on(async {
        let deadline = futures_timer(Duration::from_secs(10));
        futures::pin_mut!(deadline);
        loop {
            futures::select! {
                ev = events.next() => match ev {
                    Some(Ok(e)) if e.kind == ResourceKind::Container && e.action == "start" && e.id == c.id => return true,
                    Some(_) => continue,
                    None => return false,
                },
                _ = deadline.as_mut() => return false,
            }
        }
    });
    assert!(got_start, "start event not received");

    // logs: both streams are tagged.
    std::thread::sleep(Duration::from_millis(500));
    let chunks: Vec<LogChunk> =
        block_on(engine.logs(&name, 100, false).map(|r| r.unwrap()).collect());
    assert!(
        chunks
            .iter()
            .any(|c| c.stream == LogStream::Stdout && c.text.contains("hello-out"))
    );
    assert!(
        chunks
            .iter()
            .any(|c| c.stream == LogStream::Stderr && c.text.contains("hello-err"))
    );

    // stats: one sample with a memory limit, plus one-shot aggregate container usage.
    let sample = block_on(engine.stats(&name).next()).unwrap().unwrap();
    assert!(sample.mem_limit > 0);
    let u1 = block_on(engine.container_usage(vec![c.id.clone()])).expect("container_usage 1");
    assert!(u1.cpu_readings.contains_key(&c.id));
    std::thread::sleep(Duration::from_millis(200));
    let u2 = block_on(engine.container_usage(vec![c.id.clone()])).expect("container_usage 2");
    assert!(aggregate_cpu_cores(&u1.cpu_readings, &u2.cpu_readings, 1).is_some_and(|v| v >= 0.0));

    // exec with a TTY: echo through the shell.
    let session = block_on(engine.exec(
        &name,
        ExecSpec {
            cmd: vec!["sh".into()],
            ..Default::default()
        },
        TtySize { cols: 80, rows: 24 },
    ))
    .unwrap();
    session
        .input
        .send(ExecInput::Resize(TtySize {
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    session
        .input
        .send(ExecInput::Bytes(b"echo $((6*7)); exit\n".to_vec()))
        .unwrap();
    let out: Vec<u8> = block_on(session.output.concat());
    assert!(
        String::from_utf8_lossy(&out).contains("42"),
        "{}",
        String::from_utf8_lossy(&out)
    );

    // inspect + conflict on removing a running container.
    let raw_inspect = block_on(engine.inspect_container(&name)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw_inspect).unwrap();
    assert_eq!(v["Name"], format!("/{name}"));
    let err = block_on(engine.container_action(
        &name,
        ContainerAction::Remove {
            force: false,
            volumes: false,
        },
    ))
    .unwrap_err();
    assert!(matches!(err, EngineError::Conflict(_)), "{err:?}");
    block_on(engine.container_action(
        &name,
        ContainerAction::Remove {
            force: true,
            volumes: true,
        },
    ))
    .unwrap();
    let err = block_on(engine.inspect_container(&name)).unwrap_err();
    assert!(matches!(err, EngineError::NotFound(_)), "{err:?}");
}

#[test]
fn volume_and_network_create_remove() {
    let Some(engine) = live() else { return };
    let name = format!("ds-live-res-{}", std::process::id());
    block_on(engine.create_volume(&name)).unwrap();
    assert!(
        block_on(engine.volumes())
            .unwrap()
            .iter()
            .any(|v| v.name == name)
    );
    block_on(engine.remove_volume(&name)).unwrap();
    block_on(engine.create_network(&name)).unwrap();
    let net = block_on(engine.networks())
        .unwrap()
        .into_iter()
        .find(|n| n.name == name)
        .unwrap();
    block_on(engine.remove_network(&net.id)).unwrap();
}

#[test]
fn run_container_publishes_mounts_and_cleans_up_a_failed_start() {
    let Some(engine) = live() else { return };
    ensure_image(&engine, "alpine:3.20");
    let pid = std::process::id();
    let (name, volume, network) = (
        format!("ds-live-run-{pid}"),
        format!("ds-live-run-vol-{pid}"),
        format!("ds-live-run-net-{pid}"),
    );
    block_on(engine.create_network(&network)).unwrap();
    let spec = RunSpec {
        image: "alpine:3.20".into(),
        name: Some(name.clone()),
        cmd: vec![
            "sh".into(),
            "-c".into(),
            "echo \"$GREETING\"; sleep 300".into(),
        ],
        env: vec!["GREETING=hello run".into()],
        ports: vec![PublishedPort {
            host_ip: Some("127.0.0.1".into()),
            host_port: None,
            container_port: 8080,
            protocol: "tcp".into(),
        }],
        // Created on the fly, as `docker run -v` does.
        volumes: vec![VolumeMount {
            volume: volume.clone(),
            target: "/data".into(),
            read_only: false,
        }],
        network: Some(network.clone()),
        restart: RestartPolicy::UnlessStopped,
    };
    let id = block_on(engine.run_container(spec.clone())).expect("run");
    let v: serde_json::Value =
        serde_json::from_str(&block_on(engine.inspect_container(&id)).unwrap()).unwrap();
    assert_eq!(v["State"]["Running"], true);
    assert_eq!(v["HostConfig"]["RestartPolicy"]["Name"], "unless-stopped");
    assert_eq!(v["Mounts"][0]["Name"], volume.as_str());
    assert!(v["NetworkSettings"]["Networks"].get(&network).is_some());

    // Duplicate reads back what was run (the image's own PATH is left out).
    let template = block_on(engine.container_template(&id)).expect("template");
    assert_eq!(template.spec, spec);
    assert!(template.skipped.is_empty(), "{:?}", template.skipped);

    let net: serde_json::Value =
        serde_json::from_str(&block_on(engine.inspect_network(&network)).unwrap()).unwrap();
    assert!(net["Containers"].get(&id).is_some(), "{net}");
    let vol: serde_json::Value =
        serde_json::from_str(&block_on(engine.inspect_volume(&volume)).unwrap()).unwrap();
    assert_eq!(vol["Name"], volume.as_str());

    // The same name again: dockerd refuses, and nothing new is left behind.
    let err = block_on(engine.run_container(spec.clone())).unwrap_err();
    assert!(matches!(err, EngineError::Conflict(_)), "{err:?}");

    // A start that fails (the command does not exist) removes what it created.
    let broken = RunSpec {
        name: Some(format!("{name}-broken")),
        cmd: vec!["/does-not-exist".into()],
        ports: vec![],
        volumes: vec![],
        network: None,
        ..spec
    };
    assert!(block_on(engine.run_container(broken)).is_err());
    let names: Vec<String> = block_on(engine.containers(true))
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(!names.contains(&format!("{name}-broken")), "{names:?}");

    block_on(engine.container_action(
        &id,
        ContainerAction::Remove {
            force: true,
            volumes: true,
        },
    ))
    .unwrap();
    block_on(engine.remove_volume(&volume)).unwrap();
    block_on(engine.remove_network(&network)).unwrap();
}

#[test]
fn compose_up_down() {
    let Some(engine) = live() else { return };
    if !block_on(compose_available()) {
        eprintln!("skipped: docker compose missing");
        return;
    }
    let dir = std::env::temp_dir().join(format!("ds-compose-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        "services:\n  app:\n    image: alpine:3.20\n    command: sleep 300\n",
    )
    .unwrap();
    let project = format!("dslive{}", std::process::id());
    let labels = ComposeLabels {
        project: project.clone(),
        service: String::new(),
        working_dir: Some(dir.display().to_string()),
        config_files: vec![dir.join("compose.yaml").display().to_string()],
    };
    let run = |cmd| {
        let out: Vec<ComposeOutput> =
            block_on(run_compose(engine.target(), &labels, cmd).collect());
        assert_eq!(out.last(), Some(&ComposeOutput::Exit(Ok(()))), "{out:?}");
    };
    run(ComposeCommand::Up);
    let list = block_on(engine.containers(false)).unwrap();
    let c = list
        .iter()
        .find(|c| c.compose.as_ref().is_some_and(|l| l.project == project))
        .expect("compose container");
    assert_eq!(c.compose.as_ref().unwrap().service, "app");
    assert_eq!(
        c.compose.as_ref().unwrap().config_files,
        labels.config_files
    );
    run(ComposeCommand::Down);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dropping_exec_session_terminates_container_shell() {
    // A-001 / R-10
    let Some(engine) = live() else { return };
    ensure_image(&engine, "alpine:3.20");
    let name = format!("ds-live-exec-drop-{}", std::process::id());
    let out = std::process::Command::new("docker")
        .args(["run", "-d", "--name", &name, "alpine:3.20", "sleep", "300"])
        .output()
        .expect("docker run");
    assert!(out.status.success());

    let mut session = block_on(engine.exec(
        &name,
        ExecSpec {
            cmd: vec!["sh".into()],
            ..Default::default()
        },
        TtySize { cols: 80, rows: 24 },
    ))
    .expect("exec start");
    session
        .input
        .send(ExecInput::Bytes(b"echo ready\n".to_vec()))
        .unwrap();
    let first = block_on(session.output.next()).expect("exec output");
    assert!(!first.is_empty());

    drop(session);

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut sh_running = true;
    while std::time::Instant::now() < deadline {
        let top = std::process::Command::new("docker")
            .args(["exec", &name, "ps", "-o", "comm"])
            .output()
            .expect("docker exec ps");
        let text = String::from_utf8_lossy(&top.stdout);
        let shells = text.lines().filter(|l| l.trim() == "sh").count();
        if shells == 0 {
            sh_running = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = block_on(engine.container_action(
        &name,
        ContainerAction::Remove {
            force: true,
            volumes: true,
        },
    ));
    assert!(
        !sh_running,
        "exec shell leaked inside container after session drop"
    );
}

/// A tiny runtime-agnostic timer so the test does not depend on tokio.
fn futures_timer(d: Duration) -> impl futures::future::FusedFuture<Output = ()> {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(d);
        let _ = tx.send(());
    });
    futures::FutureExt::fuse(async move {
        let _ = rx.await;
    })
}

fn docker(args: &[&str]) -> String {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .expect("docker");
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn update_replaces_a_container_from_its_moved_tag_and_keeps_its_data() {
    let Some(engine) = live() else { return };
    ensure_image(&engine, "alpine:3.20");
    ensure_image(&engine, "alpine:3.19");
    let pid = std::process::id();
    let (name, tag, network) = (
        format!("ds-live-update-{pid}"),
        format!("ds-live-update:{pid}"),
        format!("ds-live-update-net-{pid}"),
    );
    docker(&["tag", "alpine:3.19", &tag]);
    docker(&["network", "create", &network]);
    // An anonymous volume, a second network with an alias, and a variable of its own.
    let old = docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "-e",
        "MODE=prod",
        "-v",
        "/keep",
        &tag,
        "sh",
        "-c",
        "[ -f /keep/mark ] || echo first > /keep/mark; sleep 300",
    ]);
    docker(&["network", "connect", "--alias", "upd", &network, &name]);
    let anon = docker(&["inspect", "-f", "{{range .Mounts}}{{.Name}}{{end}}", &name]);

    // Moving the tag makes the container out of date; the pull of the local-only tag
    // fails, and the newer local image is used.
    docker(&["tag", "alpine:3.20", &tag]);
    let checks = block_on(engine.check_updates(vec![old.clone()])).expect("check");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].freshness, ImageFreshness::Pulled, "{checks:?}");
    let outcome = block_on(engine.update_container(&name)).expect("update");
    let UpdateOutcome::Updated { id, leftover } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(leftover, None);
    assert_ne!(id, old);
    let v: serde_json::Value =
        serde_json::from_str(&block_on(engine.inspect_container(&name)).unwrap()).unwrap();
    assert_eq!(v["Id"], id.as_str());
    assert_eq!(v["State"]["Running"], true);
    assert_eq!(v["Config"]["Image"], tag.as_str());
    assert!(
        v["Config"]["Env"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "MODE=prod"),
        "{v}"
    );
    assert_eq!(v["Mounts"][0]["Name"], anon.as_str());
    let aliases = &v["NetworkSettings"]["Networks"][&network]["Aliases"];
    assert!(
        aliases.as_array().unwrap().iter().any(|a| a == "upd"),
        "{aliases}"
    );
    // The old container is gone.
    let names: Vec<String> = block_on(engine.containers(true))
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with(&format!("{name}-replaced"))),
        "{names:?}"
    );

    // Nothing moved since: the pull fails (local-only tag) and nothing is replaced.
    assert!(block_on(engine.update_container(&name)).is_err());
    let again: serde_json::Value =
        serde_json::from_str(&block_on(engine.inspect_container(&name)).unwrap()).unwrap();
    assert_eq!(again["Id"], id.as_str());

    // Leaving and joining a network applies to the running container.
    let attached = |engine: &Engine| {
        let v: serde_json::Value =
            serde_json::from_str(&block_on(engine.inspect_container(&name)).unwrap()).unwrap();
        v["NetworkSettings"]["Networks"].get(&network).is_some()
    };
    block_on(engine.disconnect_network(&network, &name)).expect("disconnect");
    assert!(!attached(&engine));
    block_on(engine.connect_network(&network, &name)).expect("connect");
    assert!(attached(&engine));

    docker(&["rm", "-f", &name]);
    docker(&["volume", "rm", &anon]);
    docker(&["network", "rm", &network]);
    docker(&["rmi", &tag]);
}
