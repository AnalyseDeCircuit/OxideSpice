use std::io::{BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use oxide_spice_helper_protocol::{
    FULL_HELPER_CAPABILITIES, HELPER_IPC_PROTOCOL_VERSION, HelperConnectOptions, HelperEndpoint,
    HelperEvent, HelperHello, HelperRequest, HelperSecret, HelperTransportSecurity, read_event,
    read_metadata, write_request,
};

const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

fn spawn_helper() -> Child {
    Command::new(env!("CARGO_BIN_EXE_oxide-spice-helper"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper")
}

fn wait_for_exit(child: &mut Child) {
    let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
    loop {
        if child.try_wait().expect("query helper status").is_some() {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("helper did not exit after its input closed");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_ack(reader: &mut impl std::io::BufRead) -> oxide_spice_helper_protocol::HelperHelloAck {
    let event = read_event(reader)
        .expect("read HelloAck")
        .expect("HelloAck exists");
    let HelperEvent::HelloAck { acknowledgement } = event else {
        panic!("first helper event was not HelloAck");
    };
    acknowledgement
}

fn current_hello() -> HelperRequest {
    HelperRequest::Hello {
        hello: HelperHello::current(FULL_HELPER_CAPABILITIES.to_vec()),
    }
}

#[test]
fn printed_metadata_matches_the_full_handshake_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_oxide-spice-helper"))
        .arg("--print-build-metadata")
        .output()
        .expect("print helper metadata");
    assert!(output.status.success());
    let metadata = read_metadata(&mut BufReader::new(output.stdout.as_slice()))
        .expect("decode helper metadata")
        .expect("helper metadata exists");
    assert_eq!(metadata.ipc_protocol_version, HELPER_IPC_PROTOCOL_VERSION);
    assert_eq!(metadata.capabilities, FULL_HELPER_CAPABILITIES);
    assert!(!metadata.helper_version.is_empty());
    assert!(!metadata.target.is_empty());
    assert!(!metadata.minimum_system_version.is_empty());
}

#[test]
fn native_client_libraries_load_without_device_services() {
    let status = Command::new(env!("CARGO_BIN_EXE_oxide-spice-helper"))
        .arg("--check-native-loads")
        .status()
        .expect("check native client libraries");
    assert!(status.success());
}

#[test]
fn full_helper_reports_complete_capabilities_then_accepts_close() {
    let mut child = spawn_helper();
    let mut stdin = child.stdin.take().expect("helper stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("helper stdout"));
    write_request(&mut stdin, &current_hello()).expect("write Hello");
    stdin.flush().expect("flush Hello");

    let acknowledgement = read_ack(&mut stdout);
    assert!(acknowledgement.compatible);
    assert_eq!(
        acknowledgement.protocol_version,
        HELPER_IPC_PROTOCOL_VERSION
    );
    assert_eq!(acknowledgement.capabilities, FULL_HELPER_CAPABILITIES);
    assert!(!acknowledgement.helper_version.is_empty());
    assert!(!acknowledgement.target.is_empty());

    write_request(&mut stdin, &HelperRequest::Close).expect("write Close");
    stdin.flush().expect("flush Close");
    drop(stdin);
    wait_for_exit(&mut child);
    assert!(child.wait().expect("collect helper status").success());
}

#[test]
fn compatible_hello_followed_by_eof_exits_cleanly() {
    let mut child = spawn_helper();
    let mut stdin = child.stdin.take().expect("helper stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("helper stdout"));
    write_request(&mut stdin, &current_hello()).expect("write Hello");
    stdin.flush().expect("flush Hello");
    assert!(read_ack(&mut stdout).compatible);
    drop(stdin);
    wait_for_exit(&mut child);
    assert!(child.wait().expect("collect helper status").success());
}

#[test]
fn incompatible_hello_rejects_pipelined_credentials_without_disclosure() {
    const SECRET_MARKER: &str = "must-not-appear-in-helper-output";
    let mut child = spawn_helper();
    let mut stdin = child.stdin.take().expect("helper stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("helper stdout"));
    let hello = HelperRequest::Hello {
        hello: HelperHello {
            protocol_version: HELPER_IPC_PROTOCOL_VERSION + 1,
            required_capabilities: Vec::new(),
        },
    };
    write_request(&mut stdin, &hello).expect("write incompatible Hello");
    let _ = write_request(
        &mut stdin,
        &HelperRequest::Connect {
            options: HelperConnectOptions {
                endpoint: HelperEndpoint::Tcp {
                    host: "127.0.0.1".to_owned(),
                    port: 5900,
                },
                ticket: HelperSecret::new(SECRET_MARKER),
                transport_security: HelperTransportSecurity::Plain,
                sasl: None,
            },
        },
    );
    let _ = stdin.flush();
    drop(stdin);

    let acknowledgement = read_ack(&mut stdout);
    assert!(!acknowledgement.compatible);
    let mut remaining_stdout = String::new();
    stdout
        .read_to_string(&mut remaining_stdout)
        .expect("read remaining stdout");
    wait_for_exit(&mut child);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("helper stderr")
        .read_to_string(&mut stderr)
        .expect("read helper stderr");
    assert!(!remaining_stdout.contains(SECRET_MARKER));
    assert!(!stderr.contains(SECRET_MARKER));
}

#[test]
fn legacy_connect_as_first_request_is_rejected_without_disclosure() {
    const SECRET_MARKER: &str = "legacy-connect-secret-marker";
    let mut child = spawn_helper();
    let mut stdin = child.stdin.take().expect("helper stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("helper stdout"));
    write_request(
        &mut stdin,
        &HelperRequest::Connect {
            options: HelperConnectOptions {
                endpoint: HelperEndpoint::Tcp {
                    host: "127.0.0.1".to_owned(),
                    port: 5900,
                },
                ticket: HelperSecret::new(SECRET_MARKER),
                transport_security: HelperTransportSecurity::Plain,
                sasl: None,
            },
        },
    )
    .expect("write legacy Connect");
    stdin.flush().expect("flush legacy Connect");
    drop(stdin);

    let event = read_event(&mut stdout)
        .expect("read protocol error")
        .expect("protocol error exists");
    assert!(matches!(event, HelperEvent::Error { .. }));
    let mut remaining_stdout = String::new();
    stdout
        .read_to_string(&mut remaining_stdout)
        .expect("read remaining stdout");
    wait_for_exit(&mut child);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("helper stderr")
        .read_to_string(&mut stderr)
        .expect("read helper stderr");
    assert!(!remaining_stdout.contains(SECRET_MARKER));
    assert!(!stderr.contains(SECRET_MARKER));
}
