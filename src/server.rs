use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_CLIENTS: usize = 32;

struct ClientAdmission {
    active: AtomicUsize,
    limit: usize,
}

impl ClientAdmission {
    fn new(limit: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            limit,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<ClientPermit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.limit).then_some(active + 1)
            })
            .ok()
            .map(|_| ClientPermit(Arc::clone(self)))
    }
}

struct ClientPermit(Arc<ClientAdmission>);

impl Drop for ClientPermit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

use crate::{
    dispatch::RuntimeRegistry,
    protocol::{ErrorCode, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, Request, Response},
};

pub struct Server {
    listener: UnixListener,
    socket: std::path::PathBuf,
}

impl Server {
    pub fn bind(socket: &Path) -> Result<Self, String> {
        if socket.exists() {
            return Err(format!("socket path already exists: {}", socket.display()));
        }
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create socket directory: {error}"))?;
        }
        let listener = UnixListener::bind(socket)
            .map_err(|error| format!("could not bind socket {}: {error}", socket.display()))?;
        if let Err(error) = fs::set_permissions(socket, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(socket);
            return Err(format!("could not restrict socket permissions: {error}"));
        }
        Ok(Self {
            listener,
            socket: socket.into(),
        })
    }

    pub fn serve(self, runtime: Arc<RuntimeRegistry>) -> Result<(), String> {
        self.serve_connections(runtime, None)
    }

    fn serve_connections(
        self,
        runtime: Arc<RuntimeRegistry>,
        connection_limit: Option<usize>,
    ) -> Result<(), String> {
        let mut accepted = 0;
        let admission = Arc::new(ClientAdmission::new(MAX_CONCURRENT_CLIENTS));
        for connection in self.listener.incoming() {
            match connection {
                Ok(mut stream) => {
                    accepted += 1;
                    let Some(permit) = admission.try_acquire() else {
                        let _ = stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
                        let _ = write_response(
                            &mut stream,
                            &Response::Error {
                                code: ErrorCode::ServerBusy,
                                message: format!(
                                    "daemon is serving the maximum of {MAX_CONCURRENT_CLIENTS} concurrent clients"
                                ),
                            },
                        );
                        if connection_limit == Some(accepted) {
                            return Ok(());
                        }
                        continue;
                    };
                    let runtime = Arc::clone(&runtime);
                    std::thread::Builder::new()
                        .name("dock-client".into())
                        .spawn(move || {
                            let _permit = permit;
                            let _ = handle_connection(stream, &runtime);
                        })
                        .map_err(|error| format!("could not start client handler: {error}"))?;
                    if connection_limit == Some(accepted) {
                        return Ok(());
                    }
                }
                Err(error) => return Err(format!("socket accept failed: {error}")),
            }
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

fn handle_connection(mut stream: UnixStream, runtime: &RuntimeRegistry) -> Result<(), String> {
    handle_connection_with_timeout(&mut stream, runtime, CLIENT_READ_TIMEOUT)
}

fn handle_connection_with_timeout(
    stream: &mut UnixStream,
    runtime: &RuntimeRegistry,
    read_timeout: Duration,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|error| format!("could not set client read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))
        .map_err(|error| format!("could not set client write timeout: {error}"))?;
    let reader_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(reader_stream);
    let hello = read_request(&mut reader, stream)?;
    match hello {
        Request::Hello(hello) if hello.version == PROTOCOL_VERSION => {
            write_response(
                stream,
                &Response::Hello {
                    version: PROTOCOL_VERSION,
                },
            )?;
        }
        Request::Hello(hello) => {
            write_response(
                stream,
                &Response::Error {
                    code: ErrorCode::ProtocolMismatch,
                    message: format!(
                        "unsupported protocol version {}; daemon requires {PROTOCOL_VERSION}",
                        hello.version
                    ),
                },
            )?;
            return Ok(());
        }
        _ => {
            write_response(
                stream,
                &Response::Error {
                    code: ErrorCode::HandshakeRequired,
                    message: "hello must be the first request".into(),
                },
            )?;
            return Ok(());
        }
    }
    loop {
        match read_request(&mut reader, stream) {
            Ok(Request::Inspect(request)) => match runtime.inspect(request.run_id.as_deref()) {
                Ok(mut snapshots) if request.run_id.is_some() => write_response(
                    stream,
                    &Response::Snapshot {
                        snapshot: snapshots.remove(0),
                    },
                )?,
                Ok(snapshots) => write_response(stream, &Response::Snapshots { snapshots })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::Dispatch(request)) => match runtime.dispatch(request) {
                Ok(snapshot) => write_response(stream, &Response::Dispatched { snapshot })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::LaunchIntoPane(request)) => match runtime.launch_into_pane(
                request.dispatch,
                request.workspace_id,
                request.pane_id,
            ) {
                Ok(snapshot) => write_response(stream, &Response::Dispatched { snapshot })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::TerminalLaunch(request)) => match runtime.terminal_launch(
                request.workspace_id,
                request.pane_id,
                request.run_id,
                request.profile,
                request.runtime_directory,
            ) {
                Ok(snapshot) => write_response(stream, &Response::Dispatched { snapshot })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::Lifecycle(request)) => match runtime
                .lifecycle(&request.run_id, request.operation)
            {
                Ok(snapshot) => write_response(
                    stream,
                    &Response::LifecycleApplied {
                        operation: request.operation,
                        snapshot,
                    },
                )?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::SubmitHandoff(request)) => match runtime.submit_handoff(request.packet) {
                Ok(record) => write_response(stream, &Response::HandoffSubmitted { record })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::ReviewInbox(_)) => match runtime.review_inbox() {
                Ok(items) => write_response(stream, &Response::ReviewInbox { items })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::Decide(request)) => {
                match runtime.decide(request.run_id, request.route, request.note) {
                    Ok(decision) => {
                        write_response(stream, &Response::DecisionRecorded { decision })?
                    }
                    Err((code, message)) => {
                        write_response(stream, &Response::Error { code, message })?
                    }
                }
            }
            Ok(Request::QueueGated(request)) => match runtime.queue_gated(
                request.dispatch,
                request.upstream_run_id,
                request.required_route,
            ) {
                Ok(gate) => write_response(stream, &Response::GateQueued { gate })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::ReleaseGate(request)) => match runtime
                .release_gate(&request.downstream_run_id)
            {
                Ok(snapshot) => write_response(stream, &Response::GateReleased { snapshot })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::InspectProgramme(_)) => write_response(
                stream,
                &Response::Programme {
                    portfolio: runtime.inspect_programme(),
                },
            )?,
            Ok(Request::Workspace(crate::protocol::WorkspaceRequest::Inspect)) => write_response(
                stream,
                &Response::Layout {
                    layout: runtime.layout(),
                },
            )?,
            Ok(Request::Workspace(request)) => match runtime.workspace(request) {
                Ok(workspace) => write_response(stream, &Response::WorkspaceChanged { workspace })?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::PaneInput(request)) => match runtime.pane_input(
                &request.workspace_id,
                &request.pane_id,
                request.input.as_bytes(),
            ) {
                Ok(bytes) => write_response(
                    stream,
                    &Response::PaneInputAccepted {
                        workspace_id: request.workspace_id,
                        pane_id: request.pane_id,
                        bytes,
                    },
                )?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            Ok(Request::Hello(_)) => {
                write_response(
                    stream,
                    &Response::Error {
                        code: ErrorCode::MalformedRequest,
                        message: "hello may only be sent once".into(),
                    },
                )?;
                return Ok(());
            }
            Err(error) if error == "connection closed" => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

fn read_request(reader: &mut impl BufRead, stream: &mut UnixStream) -> Result<Request, String> {
    let mut bytes = Vec::new();
    let count = (&mut *reader)
        .take(MAX_MESSAGE_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                let _ = write_response(
                    stream,
                    &Response::Error {
                        code: ErrorCode::RequestTimeout,
                        message: "client did not complete a request before the read deadline"
                            .into(),
                    },
                );
                "request timed out".into()
            } else {
                format!("could not read request: {error}")
            }
        })?;
    if count == 0 {
        return Err("connection closed".into());
    }
    if count as u64 > MAX_MESSAGE_BYTES || !bytes.ends_with(b"\n") {
        write_response(
            stream,
            &Response::Error {
                code: ErrorCode::RequestTooLarge,
                message: format!(
                    "request must be newline-terminated and at most {MAX_MESSAGE_BYTES} bytes"
                ),
            },
        )?;
        return Err("request too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        let _ = write_response(
            stream,
            &Response::Error {
                code: ErrorCode::MalformedRequest,
                message: format!("invalid request: {error}"),
            },
        );
        "malformed request".into()
    })
}

fn write_response(stream: &mut UnixStream, response: &Response) -> Result<(), String> {
    serde_json::to_writer(&mut *stream, response).map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::Shutdown,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    static SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn registry() -> RuntimeRegistry {
        RuntimeRegistry::new(
            std::env::current_dir()
                .unwrap()
                .join("target")
                .join(format!(
                    "dock-registry-test-{}-{}",
                    std::process::id(),
                    SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                )),
            64,
        )
        .expect("test registry")
    }

    fn socket_path() -> PathBuf {
        std::env::current_dir()
            .expect("current directory")
            .join("target")
            .join(format!(
                "dock-listener-test-{}-{}.sock",
                std::process::id(),
                SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ))
    }

    fn connect(path: &Path) -> UnixStream {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => return stream,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("could not connect to {}: {error}", path.display()),
            }
        }
    }

    fn socket_exchange(path: &Path, lines: &[&str]) -> Vec<Response> {
        let mut client = connect(path);
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        for line in lines {
            client.write_all(line.as_bytes()).expect("write request");
            client.write_all(b"\n").expect("write newline");
        }
        client.shutdown(Shutdown::Write).expect("finish requests");
        BufReader::new(client)
            .lines()
            .map(|line| serde_json::from_str(&line.expect("response line")).expect("response"))
            .collect()
    }

    fn exchange(lines: &[&str], runtime: &RuntimeRegistry) -> Vec<Response> {
        let (mut client, server) = UnixStream::pair().expect("socket pair");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                handle_connection(server, runtime).ok();
            });
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            for line in lines {
                client.write_all(line.as_bytes()).expect("write request");
                client.write_all(b"\n").expect("write newline");
            }
            client.shutdown(Shutdown::Write).expect("finish requests");
            let reader = BufReader::new(client);
            reader
                .lines()
                .map(|line| serde_json::from_str(&line.expect("response line")).expect("response"))
                .collect()
        })
    }

    #[test]
    fn version_mismatch_and_malformed_input_fail_safely() {
        let runtime = registry();
        assert!(matches!(
            exchange(&[r#"{"type":"hello","version":999}"#], &runtime).as_slice(),
            [Response::Error {
                code: ErrorCode::ProtocolMismatch,
                ..
            }]
        ));
        assert!(matches!(
            exchange(&["not-json"], &runtime).as_slice(),
            [Response::Error {
                code: ErrorCode::MalformedRequest,
                ..
            }]
        ));
    }

    #[test]
    fn newer_hello_fields_reach_version_negotiation_while_inspect_stays_strict() {
        let runtime = registry();
        assert!(matches!(
            exchange(
                &[r#"{"type":"hello","version":999,"future":true}"#],
                &runtime
            )
            .as_slice(),
            [Response::Error {
                code: ErrorCode::ProtocolMismatch,
                ..
            }]
        ));
        assert!(matches!(
            exchange(
                &[
                    r#"{"type":"hello","version":6,"future":true}"#,
                    r#"{"type":"inspect","future":true}"#
                ],
                &runtime
            )
            .as_slice(),
            [
                Response::Hello {
                    version: PROTOCOL_VERSION
                },
                Response::Error {
                    code: ErrorCode::MalformedRequest,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn incomplete_request_times_out_with_a_protocol_error() {
        let runtime = registry();
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                assert_eq!(
                    handle_connection_with_timeout(
                        &mut server,
                        &runtime,
                        Duration::from_millis(25)
                    ),
                    Err("request timed out".into())
                );
            });
            client.write_all(b"{").expect("partial request");
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("read timeout");
            let mut line = String::new();
            BufReader::new(client)
                .read_line(&mut line)
                .expect("timeout response line");
            let response: Response = serde_json::from_str(&line).expect("timeout response");
            assert!(matches!(
                response,
                Response::Error {
                    code: ErrorCode::RequestTimeout,
                    ..
                }
            ));
        });
    }

    #[test]
    fn admission_limit_accounts_for_release_without_exceeding_capacity() {
        let admission = Arc::new(ClientAdmission::new(2));
        let first = admission.try_acquire().expect("first permit");
        let second = admission.try_acquire().expect("second permit");
        assert!(admission.try_acquire().is_none());
        assert_eq!(admission.active.load(Ordering::Acquire), 2);
        drop(first);
        let replacement = admission.try_acquire().expect("released permit");
        assert_eq!(admission.active.load(Ordering::Acquire), 2);
        drop((second, replacement));
        assert_eq!(admission.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn reconnecting_clients_observe_the_same_owned_process() {
        let runtime = registry();
        let root = std::env::current_dir().unwrap().display().to_string();
        let dispatch =
            serde_json::to_string(&Request::Dispatch(crate::protocol::DispatchRequest {
                repository_root: root.clone(),
                external_task_ref: "TASK-SOCKET".into(),
                run_id: "dock_socket_reconnect".into(),
                worktree: root,
                adapter: crate::adapter::AdapterSelection {
                    id: crate::adapter::AdapterId::Fixture,
                    executable: None,
                    arguments: vec!["-c".into(), "sleep 2".into()],
                },
            }))
            .unwrap();
        let hello = r#"{"type":"hello","version":6}"#;
        let dispatched = exchange(&[hello, &dispatch], &runtime);
        let pid = match &dispatched[1] {
            Response::Dispatched { snapshot } => snapshot.pid,
            response => panic!("unexpected response: {response:?}"),
        };
        assert!(pid.is_some());
        let inspect = r#"{"type":"inspect","run_id":"dock_socket_reconnect"}"#;
        let first = exchange(&[hello, inspect], &runtime);
        let second = exchange(&[hello, inspect], &runtime);
        let inspected_pid = |responses: &[Response]| match &responses[1] {
            Response::Snapshot { snapshot } => snapshot.pid,
            response => panic!("unexpected response: {response:?}"),
        };
        assert_eq!(inspected_pid(&first), pid);
        assert_eq!(inspected_pid(&first), inspected_pid(&second));
    }

    #[test]
    fn real_listener_lifecycle_permissions_reconnect_and_malformed_isolation() {
        let socket = socket_path();
        let server = match Server::bind(&socket) {
            Ok(server) => server,
            Err(error) if error.contains("Operation not permitted") => {
                eprintln!("skipping real listener smoke: sandbox denied Unix socket bind: {error}");
                return;
            }
            Err(error) => panic!("bind listener: {error}"),
        };
        assert_eq!(
            fs::metadata(&socket)
                .expect("socket metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let runtime = Arc::new(registry());
        let server_runtime = Arc::clone(&runtime);
        let server_thread = std::thread::spawn(move || {
            server
                .serve_connections(server_runtime, Some(3))
                .expect("serve three clients");
        });

        assert!(matches!(
            socket_exchange(&socket, &["not-json"]).as_slice(),
            [Response::Error {
                code: ErrorCode::MalformedRequest,
                ..
            }]
        ));
        let requests = [r#"{"type":"hello","version":6}"#, r#"{"type":"inspect"}"#];
        let first = socket_exchange(&socket, &requests);
        let second = socket_exchange(&socket, &requests);
        let snapshot_count = |responses: &[Response]| match &responses[1] {
            Response::Snapshots { snapshots } => snapshots.len(),
            response => panic!("unexpected response: {response:?}"),
        };
        assert_eq!(snapshot_count(&first), 0);
        assert_eq!(snapshot_count(&first), snapshot_count(&second));

        server_thread.join().expect("listener thread");
        assert!(
            !socket.exists(),
            "socket must be removed when listener drops"
        );
        drop(runtime);
    }

    #[test]
    fn workspace_socket_lifecycle_is_end_to_end() {
        let runtime = registry();
        let responses = exchange(
            &[
                r#"{"type":"hello","version":6}"#,
                r#"{"type":"workspace","operation":"create","workspace_id":"daily","name":"Daily","pane_id":"pane_one"}"#,
                r#"{"type":"workspace","operation":"split","workspace_id":"daily","pane_id":"pane_one","new_pane_id":"pane_two","axis":"vertical"}"#,
                r#"{"type":"workspace","operation":"focus","workspace_id":"daily","pane_id":"pane_one"}"#,
                r#"{"type":"workspace","operation":"inspect"}"#,
            ],
            &runtime,
        );
        assert!(
            matches!(&responses[1], Response::WorkspaceChanged { workspace: Some(workspace) } if workspace.panes.len()==1)
        );
        assert!(
            matches!(&responses[2], Response::WorkspaceChanged { workspace: Some(workspace) } if workspace.panes.len()==2)
        );
        assert!(
            matches!(&responses[4], Response::Layout { layout } if layout.workspaces[0].focused_pane_id=="pane_one")
        );
    }
}
