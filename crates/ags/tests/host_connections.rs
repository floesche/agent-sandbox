use std::path::Path;

use ags::config::{parse_and_validate_with_overlay, parse_toml_str};
use ags::hooks::{Contributions, validate_response};

const BASE: &str = r#"
[sandbox]
image = "test"
containerfile = "/tmp/Containerfile"
cache_dir = "/tmp/cache"
gitconfig_path = "/tmp/gitconfig"
auth_key = "/tmp/auth"
sign_key = "/tmp/sign"
"#;
const CONNECTION: &str = r#"
[[host_connection]]
name = "service"
host_tcp = "127.0.0.1:4788"
sandbox_tcp = "127.0.0.1:4788"
enabled = false
allow_hooks = true
"#;
fn parse(extra: &str) -> Result<ags::config::ValidatedConfig, ags::config::ConfigError> {
    parse_toml_str(&format!("{BASE}{extra}"), Path::new("/tmp/host.toml"))
}
fn options(lockdown: bool) -> ags::cli::RunOptions {
    // Exercise the public CLI rather than duplicating launch option defaults.
    let args = if lockdown {
        vec!["ags", "--agent", "shell", "--lockdown"]
    } else {
        vec!["ags", "--agent", "shell"]
    };
    match ags::cli::parse_args(args.into_iter().map(String::from)).unwrap() {
        ags::cli::Command::Run(options) => options,
        _ => unreachable!(),
    }
}

#[test]
fn config_accepts_stream_endpoints_and_rejects_unsafe_or_ambiguous_definitions() {
    assert!(parse(CONNECTION).is_ok());
    assert!(parse(&CONNECTION.replace("127.0.0.1:4788", "[::1]:4788")).is_ok());
    for (from, to) in [
        ("127.0.0.1:4788", "0.0.0.0:4788"),
        ("127.0.0.1:4788", "localhost:4788"),
        ("127.0.0.1:4788", "192.0.2.1:4788"),
        ("127.0.0.1:4788", "127.0.0.1:0"),
        ("name = \"service\"", "name = \"../service\""),
        ("enabled = false", "enable = false"),
        (
            "host_tcp = \"127.0.0.1:4788\"",
            "host_unix = \"relative/socket\"",
        ),
        (
            "sandbox_tcp = \"127.0.0.1:4788\"",
            "sandbox_unix = \"/ssh-agent\"",
        ),
        (
            "sandbox_tcp = \"127.0.0.1:4788\"",
            "sandbox_unix = \"/run/ags-connections/../bad.sock\"",
        ),
    ] {
        assert!(parse(&CONNECTION.replace(from, to)).is_err(), "{to}");
    }
    assert!(parse(&format!("{CONNECTION}{CONNECTION}")).is_err());
    assert!(parse(&format!("{CONNECTION}host_unix = \"/run/test.sock\"\n")).is_err());
    let unix = CONNECTION
        .replace(
            "host_tcp = \"127.0.0.1:4788\"",
            "host_unix = \"/run/service.sock\"",
        )
        .replace(
            "sandbox_tcp = \"127.0.0.1:4788\"",
            "sandbox_unix = \"/run/ags-connections/service.sock\"",
        );
    assert!(parse(&unix).is_ok());
}

#[test]
fn overlays_cannot_define_or_replace_host_grants() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("host.toml");
    let overlay = dir.path().join("project.toml");
    std::fs::write(&base, format!("{BASE}{CONNECTION}")).unwrap();
    for text in [CONNECTION, "host_connection = []"] {
        std::fs::write(&overlay, text).unwrap();
        let error = parse_and_validate_with_overlay(&base, Some(&overlay)).unwrap_err();
        assert!(error.to_string().contains("cannot define host_connection"));
    }
}

#[test]
fn hooks_request_only_granted_names_and_never_open_sockets_during_materialization() {
    let mut config = parse(CONNECTION).unwrap();
    let response = validate_response(
        br#"{"version":1,"connections":["service","service"],"env":{"SERVICE_KEY":"sentinel"}}"#,
    )
    .unwrap();
    let contributions = Contributions::merge(vec![response]).unwrap();
    assert!(
        !contributions
            .redact_summary()
            .to_string()
            .contains("sentinel")
    );
    let materialized = contributions
        .materialize(&mut config, &options(false))
        .unwrap();
    assert!(config.host_connections[0].enabled);
    assert_eq!(
        materialized.env,
        vec![("SERVICE_KEY".into(), "sentinel".into())]
    );
    for (declaration, request) in [
        (
            CONNECTION.replace("allow_hooks = true", "allow_hooks = false"),
            "service",
        ),
        (CONNECTION.to_owned(), "unknown"),
    ] {
        let mut config = parse(&declaration).unwrap();
        let json = format!(r#"{{"version":1,"connections":["{request}"]}}"#);
        assert!(
            Contributions::merge(vec![validate_response(json.as_bytes()).unwrap()])
                .unwrap()
                .materialize(&mut config, &options(false))
                .is_err()
        );
    }
}

#[test]
fn lockdown_rejects_hook_connection_requests() {
    let mut config = parse(CONNECTION).unwrap();
    let contributions = Contributions::merge(vec![
        validate_response(br#"{"version":1,"connections":["service"]}"#).unwrap(),
    ])
    .unwrap();
    assert!(
        contributions
            .materialize(&mut config, &options(true))
            .err()
            .unwrap()
            .contains("--lockdown")
    );
}

#[test]
fn container_client_socket_and_process_tests() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../agent/tests/host-connection-client.test.py");
    let output = std::process::Command::new("python3")
        .arg(script)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hook_protocol_has_no_endpoint_or_process_escape_hatch() {
    for json in [
        r#"{"version":1,"connections":[{"host_tcp":"127.0.0.1:4788"}]}"#,
        r#"{"version":1,"connections":["../secret"]}"#,
        r#"{"version":1,"connections":["service"],"command":["sh"]}"#,
    ] {
        assert!(validate_response(json.as_bytes()).is_err());
    }
}
