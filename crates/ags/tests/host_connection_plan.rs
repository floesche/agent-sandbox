use ags::config::{MountMode, parse_toml_str};
use ags::host_connections::{Connection, Session};
use ags::plan::{LaunchPlan, PlanEnv, PlanMount, SecurityConfig, WorkdirMapping};
use std::path::Path;

fn plan() -> LaunchPlan {
    LaunchPlan {
        runtime_lease: None,
        workspace_cache_lease: None,
        image: "test".into(),
        pnpm_version: Default::default(),
        extra_dnf_packages: vec![],
        tool_downloads: vec![],
        container_name: "test".into(),
        workdir: WorkdirMapping {
            host: "/work".into(),
            container: "/work".into(),
        },
        mounts: vec![],
        env: PlanEnv {
            inline: vec![],
            passthrough_names: vec![],
            env_file_entries: vec![],
            read_roots_json: "[]".into(),
            write_roots_json: "[]".into(),
        },
        security: SecurityConfig::default(),
        network_mode: "pasta".into(),
        boot_dirs: vec![],
        entrypoint: "exec bash \"$@\"".into(),
        payload_fd_count: 0,
        bootstrap_path: None,
    }
}

#[test]
fn connections_mount_read_only_and_tcp_wraps_existing_entrypoint() {
    let root = tempfile::tempdir().unwrap();
    let host = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let connection = Connection {
        name: "demo".into(),
        host_unix: None,
        host_tcp: Some(host.local_addr().unwrap()),
        sandbox_unix: None,
        sandbox_tcp: Some("127.0.0.1:4788".parse().unwrap()),
        enabled: true,
        allow_hooks: false,
    };
    let session = Session::start(&[connection], root.path()).unwrap();
    let mut plan = plan();
    session.attach(&mut plan).unwrap();
    assert_eq!(plan.mounts.len(), 1);
    assert_eq!(plan.mounts[0].mode, MountMode::Ro);
    assert_eq!(plan.mounts[0].container, "/run/ags-connections");
    assert!(plan.entrypoint.contains("/usr/bin/python3 -I"));
    assert!(plan.entrypoint.ends_with("_ \"$@\""));
    assert!(
        plan.entrypoint
            .contains(&ags::util::shell_quote("exec bash \"$@\""))
    );
    assert_eq!(
        plan.security.userns.as_deref(),
        Some("keep-id:uid=1000,gid=1000")
    );
    assert!(session.attach(&mut plan).unwrap_err().contains("overlap"));
}

#[test]
fn connection_runtime_cannot_be_masked_by_another_mount() {
    let root = tempfile::tempdir().unwrap();
    let session = Session::start(&[], root.path()).unwrap();
    for destination in [
        "/",
        "/run",
        "/run/ags-connections",
        "/run/ags-connections/test.sock",
        "/var/run/ags-connections",
        "/tmp/../run/ags-connections",
    ] {
        let mut plan = plan();
        plan.mounts.push(PlanMount {
            host: "/tmp".into(),
            container: destination.into(),
            mode: MountMode::Ro,
        });
        assert!(session.attach(&mut plan).is_err(), "{destination}");
    }
}

#[test]
fn hook_literals_use_private_env_file_not_podman_argv_and_cli_still_wins() {
    use ags::hooks::{Contributions, validate_response};
    let mut config = parse_toml_str(
        r#"
[sandbox]
image="test"
containerfile="/tmp/Containerfile"
cache_dir="/tmp/cache"
gitconfig_path="/tmp/gitconfig"
auth_key="/tmp/auth"
sign_key="/tmp/sign"
"#,
        Path::new("/tmp/config.toml"),
    )
    .unwrap();
    let ags::cli::Command::Run(options) = ags::cli::parse_args(
        ["ags", "--agent", "shell", "--env", "OVERRIDE=cli"].map(String::from),
    )
    .unwrap() else {
        panic!()
    };
    let contributions = Contributions::merge(vec![
        validate_response(
            br#"{"version":1,"env":{"SERVICE_KEY":"private-hook-sentinel","OVERRIDE":"hook"}}"#,
        )
        .unwrap(),
    ])
    .unwrap();
    let materialized = contributions.materialize(&mut config, &options).unwrap();
    let mut plan = plan();
    plan.env.inline = materialized.env.clone();
    plan.env
        .env_file_entries
        .push(("SERVICE_KEY".into(), "old".into()));
    materialized.finish_plan_env(&mut plan);
    assert_eq!(
        plan.env.env_file_entries,
        vec![("SERVICE_KEY".into(), "private-hook-sentinel".into())]
    );
    assert_eq!(plan.env.inline, vec![("OVERRIDE".into(), "cli".into())]);
    let args = ags::podman::build_run_args(&plan, Path::new("/tmp/env"));
    assert!(!args.join(" ").contains("private-hook-sentinel"));
}
