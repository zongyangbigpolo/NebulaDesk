//! Invalid launches are rejected before a window or network connection exists.
use std::io::Write;
use std::process::{Command, Stdio};

fn launch(input: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nebula-client"))
        .arg("desktop-session")
        .env_remove("NEBULA_MANAGER_URL")
        .env_remove("NEBULA_TENANT")
        .env_remove("NEBULA_EMAIL")
        .env_remove("NEBULA_PASSWORD")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    if let Err(error) = stdin.write_all(input) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    drop(stdin);
    child.wait_with_output().unwrap()
}

#[test]
fn desktop_mode_does_not_require_login_and_eof_fails_cleanly() {
    let result = launch(b"");
    assert!(!result.status.success());
    let event: nebula_desktop_protocol::Event = serde_json::from_slice(&result.stdout).unwrap();
    assert!(matches!(
        event,
        nebula_desktop_protocol::Event::State {
            state: nebula_desktop_protocol::SessionState::Failed,
            ..
        }
    ));
    assert!(!String::from_utf8_lossy(&result.stderr).contains("--manager-url"));
}

#[test]
fn launch_parser_never_echoes_secret_values_to_either_pipe() {
    let result = launch(b"{\"version\":\"TOP_SECRET_BEARER\"}\n");
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("TOP_SECRET_BEARER"));
    assert!(!String::from_utf8_lossy(&result.stderr).contains("TOP_SECRET_BEARER"));
}

#[test]
fn invalid_ticket_id_and_protocol_version_are_rejected_before_connecting() {
    for version in [1, 2] {
        let input = format!(
            "{{\"version\":{version},\"resource_id\":\"r\",\"resource_name\":\"desktop\",\"ticket\":{{\"session_id\":\"invalid-id\",\"ticket\":\"TOP_SECRET_BEARER\",\"gateway_addr\":\"localhost:1\",\"gateway_pin\":\"00\",\"agent_key\":\"00\"}}}}\n"
        );
        let result = launch(input.as_bytes());
        assert!(!result.status.success());
        assert!(!String::from_utf8_lossy(&result.stdout).contains("TOP_SECRET_BEARER"));
        assert!(!String::from_utf8_lossy(&result.stderr).contains("TOP_SECRET_BEARER"));
    }
}

#[test]
fn oversized_launch_is_rejected_without_waiting_for_a_newline() {
    let input = vec![b'x'; nebula_desktop_protocol::MAX_LINE_BYTES + 1];
    let result = launch(&input);
    assert!(!result.status.success());
    assert!(result.stdout.len() < 512);
    assert!(result.stderr.len() < 512);
}
