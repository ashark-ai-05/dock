use std::{
    collections::HashMap,
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
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// How long a request that has *already started arriving* may take to finish.
///
/// Short on purpose: once the first byte is in, the rest of a line is microseconds away on a
/// local socket, so a message that stalls half-sent is a fault and must not pin a connection
/// or the admission slot behind it.
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a client may sit *between* requests without sending anything.
///
/// This is not a protocol deadline. A dashboard holds its request connection open across
/// every pause its user takes, and that connection has no reconnect path, so closing it under
/// an idle client ends the session — which is exactly what a five-second bound did. The only
/// job left for a bound here is to stop a *wedged* client (alive, but never sending again)
/// from holding two of the 32 admission slots for the daemon's entire lifetime; a client that
/// dies is reclaimed immediately by EOF, not by this. So it is set far past any plausible
/// unattended session — a long weekend away from the machine included — while still
/// guaranteeing the slots come back without operator action.
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_CLIENTS: usize = 32;

/// The two deadlines a client read is subject to.
///
/// They are separate because waiting for the first byte of a new request and waiting for the
/// rest of a message already in flight are different conditions: only the second is a fault.
/// Conflating them is what made an idle dashboard look like a protocol violation.
#[derive(Clone, Copy)]
struct ReadTimeouts {
    idle: Duration,
    in_flight: Duration,
}

impl ReadTimeouts {
    const PRODUCTION: Self = Self {
        idle: CLIENT_IDLE_TIMEOUT,
        in_flight: CLIENT_REQUEST_TIMEOUT,
    };

    /// The handshake is not an idle wait: a client that has just connected is expected to say
    /// hello at once, so a connection that never speaks at all gives its slot back in seconds
    /// rather than days.
    fn handshake(self) -> Self {
        Self {
            idle: self.in_flight,
            in_flight: self.in_flight,
        }
    }
}

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
    detect::{AgentKind, AgentState},
    dispatch::RuntimeRegistry,
    protocol::{
        ErrorCode, Event, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, ProcessState, Request, Response,
    },
    terminal::ScreenSync,
};

/// How often the streaming loop samples the live emulators. Fast enough that a keystroke
/// echoes without a perceptible lag, and free when nothing changed because an unchanged
/// screen yields an empty delta and therefore no frame.
const STREAM_POLL_INTERVAL: Duration = Duration::from_millis(16);

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
    // `None`: a real subscriber streams until it disconnects. A deadline here would silently
    // stop the push loop and freeze the dashboard with no error to explain it.
    handle_connection_with_timeout(&mut stream, runtime, ReadTimeouts::PRODUCTION, None)
}

fn handle_connection_with_timeout(
    stream: &mut UnixStream,
    runtime: &RuntimeRegistry,
    timeouts: ReadTimeouts,
    stream_deadline: Option<Instant>,
) -> Result<(), String> {
    // No read timeout is set here: `read_request` chooses one per read, because which bound
    // applies depends on whether a message has started arriving.
    stream
        .set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))
        .map_err(|error| format!("could not set client write timeout: {error}"))?;
    let reader_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(reader_stream);
    let hello = read_request(&mut reader, stream, timeouts.handshake())?;
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
        match read_request(&mut reader, stream, timeouts) {
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
            // `input` has carried base64 since protocol v7 so escape sequences and high bytes
            // survive JSON transport; decoding here is what makes the daemon actually speak it.
            Ok(Request::PaneInput(request)) => match request.decode() {
                Ok(input) => {
                    match runtime.pane_input(&request.workspace_id, &request.pane_id, &input) {
                        Ok(bytes) => write_response(
                            stream,
                            &Response::PaneInputAccepted {
                                workspace_id: request.workspace_id,
                                pane_id: request.pane_id,
                                bytes,
                            },
                        )?,
                        Err((code, message)) => {
                            write_response(stream, &Response::Error { code, message })?
                        }
                    }
                }
                Err(message) => write_response(
                    stream,
                    &Response::Error {
                        code: ErrorCode::InvalidBinding,
                        message,
                    },
                )?,
            },
            Ok(Request::PaneResize(request)) => match runtime.pane_resize(
                &request.workspace_id,
                &request.pane_id,
                request.rows,
                request.cols,
            ) {
                Ok(()) => write_response(stream, &Response::Ack)?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
            // Subscribing converts the connection into a one-way push channel: the client
            // sends nothing more on it, so this handler never returns to the request loop.
            Ok(Request::Subscribe(_)) => return stream_events(stream, runtime, stream_deadline),
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

/// Pushes screen deltas to one subscriber until it disconnects.
///
/// Each run gets a `ScreenSync` recording what *this* subscriber has already been sent, so
/// the loop transmits only the difference against that view: an unchanged pane produces an
/// empty delta and therefore no frame at all, which is the entire point of pushing instead
/// of answering `Inspect` polls with every run's full scrollback.
///
/// `deadline` is `None` in production. Tests pass a short one because the loop is otherwise
/// unbounded and a test driving it through a fixed request list would never observe an end.
fn stream_events(
    stream: &mut UnixStream,
    runtime: &RuntimeRegistry,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let mut syncs: HashMap<String, SubscriberView> = HashMap::new();
    // Kept outside `syncs` so a re-seed cannot restart the numbering: Task 11's client reads a
    // revision that moves backwards as a dropped frame and asks to re-attach, which would loop.
    let mut revisions: HashMap<String, u64> = HashMap::new();
    let mut states: HashMap<String, (Option<AgentKind>, AgentState)> = HashMap::new();
    // Tracked separately from `states` because a plain shell has no agent: its identity stays
    // `None` and its agent state stays `Idle` forever, so an exit would never reach a
    // subscriber through `AgentStateChanged`. Its screen stops changing at the same moment,
    // so no delta carries the news either. Without this the pane renders its dead last frame
    // and reports Running until something else happens to issue a request.
    let mut process_states: HashMap<String, ProcessState> = HashMap::new();
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(());
        }
        for snapshot in runtime.inspect(None).unwrap_or_default() {
            // A resize invalidates the row-by-row diff (vt100 zips the two grids, so rows
            // beyond the smaller one would never be transmitted). Re-seed from a full frame
            // instead of diffing across a geometry change.
            if syncs
                .get(&snapshot.run_id)
                .is_some_and(|view| view.size != (snapshot.rows, snapshot.cols))
            {
                syncs.remove(&snapshot.run_id);
            }
            let attached = syncs.contains_key(&snapshot.run_id);
            // Borrowed immutably: the sync must not advance until the bytes are on the wire.
            let delta = runtime.with_run_screen(&snapshot.run_id, |screen| {
                match syncs.get(&snapshot.run_id) {
                    Some(view) => view.sync.delta_from(screen),
                    None => screen.state_bytes(),
                }
            });
            // No live runtime to read: leave this run unattached so it still gets a full
            // snapshot rather than a delta if it becomes readable later.
            let Some(delta) = delta else { continue };
            if !delta.is_empty() {
                let revision = revisions.get(&snapshot.run_id).copied().unwrap_or(0) + 1;
                let encoded = STANDARD.encode(&delta);
                let event = if attached {
                    Event::PaneDelta {
                        run_id: snapshot.run_id.clone(),
                        revision,
                        bytes: encoded,
                    }
                } else {
                    Event::PaneAttached {
                        run_id: snapshot.run_id.clone(),
                        revision,
                        rows: snapshot.rows,
                        cols: snapshot.cols,
                        screen: encoded,
                    }
                };
                // Advance this subscriber's view only once the write succeeded, so a failed
                // write leaves it consistent with what the client actually received.
                write_response(stream, &Response::Stream { event })?;
                syncs
                    .entry(snapshot.run_id.clone())
                    .or_insert_with(|| SubscriberView::new(snapshot.rows, snapshot.cols))
                    .sync
                    .apply(&delta);
                revisions.insert(snapshot.run_id.clone(), revision);
            }
            if process_states.get(&snapshot.run_id) != Some(&snapshot.state) {
                write_response(
                    stream,
                    &Response::Stream {
                        event: Event::PaneState {
                            run_id: snapshot.run_id.clone(),
                            state: snapshot.state.clone(),
                        },
                    },
                )?;
                // Recorded only after the write succeeded, so a failed write cannot convince
                // this loop that the subscriber already knows.
                process_states.insert(snapshot.run_id.clone(), snapshot.state.clone());
            }
            let current = (snapshot.agent, snapshot.agent_state);
            if states.get(&snapshot.run_id) != Some(&current) {
                write_response(
                    stream,
                    &Response::Stream {
                        event: Event::AgentStateChanged {
                            run_id: snapshot.run_id.clone(),
                            agent: current.0,
                            state: current.1,
                        },
                    },
                )?;
                states.insert(snapshot.run_id, current);
            }
        }
        thread::sleep(STREAM_POLL_INTERVAL);
    }
}

/// The screen one subscriber has already been sent for one run, and the geometry that view
/// was built at. Revisions live outside this so dropping it to re-seed cannot reset them.
struct SubscriberView {
    sync: ScreenSync,
    size: (u16, u16),
}

impl SubscriberView {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            sync: ScreenSync::new(rows, cols),
            size: (rows, cols),
        }
    }
}

/// Blocks until the first byte of a request has been buffered, under the idle bound.
///
/// Buffering the byte here rather than peeking is what makes the split work: everything the
/// caller reads afterwards is either already in hand or part of a message the client has
/// demonstrably begun sending, so the short in-flight bound can apply to all of it.
fn await_request_start(
    reader: &mut impl BufRead,
    stream: &mut UnixStream,
    timeouts: ReadTimeouts,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(timeouts.idle))
        .map_err(|error| format!("could not set client idle timeout: {error}"))?;
    loop {
        match reader.fill_buf() {
            // The peer is gone: reclaiming the connection here is what keeps an abandoned
            // client cheap without any help from a deadline.
            Ok([]) => return Err("connection closed".into()),
            Ok(_) => return Ok(()),
            // `BufReader` surfaces `Interrupted` instead of retrying it, and a signal
            // delivered to the daemon must not be mistaken for a silent client.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(report_read_failure(
                    stream,
                    error,
                    "client sent nothing for the idle timeout",
                ));
            }
        }
    }
}

/// Turns a failed client read into the error the connection loop ends on, answering a timeout
/// with the protocol's own `RequestTimeout` first so the client learns why it was dropped.
fn report_read_failure(stream: &mut UnixStream, error: std::io::Error, message: &str) -> String {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        let _ = write_response(
            stream,
            &Response::Error {
                code: ErrorCode::RequestTimeout,
                message: message.into(),
            },
        );
        "request timed out".into()
    } else {
        format!("could not read request: {error}")
    }
}

fn read_request(
    reader: &mut impl BufRead,
    stream: &mut UnixStream,
    timeouts: ReadTimeouts,
) -> Result<Request, String> {
    await_request_start(reader, stream, timeouts)?;
    // A message is now in flight, so the short bound takes over for the rest of the line.
    stream
        .set_read_timeout(Some(timeouts.in_flight))
        .map_err(|error| format!("could not set client read timeout: {error}"))?;
    let mut bytes = Vec::new();
    let count = (&mut *reader)
        .take(MAX_MESSAGE_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| {
            report_read_failure(
                stream,
                error,
                "client did not complete a request before the read deadline",
            )
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
    use crate::protocol::{HelloRequest, PaneInputRequest, PaneResizeRequest, SubscribeRequest};
    use std::{
        net::Shutdown,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    static SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// Every pane auto-launches a real `$SHELL` process group and `RuntimeRegistry` does not
    /// reap on drop, so a bare registry would leak a login shell per pane that outlives the
    /// test binary. This guard retires every run it still owns and removes its state directory.
    struct TestRegistry {
        registry: Arc<RuntimeRegistry>,
        state: PathBuf,
    }

    impl std::ops::Deref for TestRegistry {
        type Target = RuntimeRegistry;
        fn deref(&self) -> &Self::Target {
            &self.registry
        }
    }

    impl TestRegistry {
        fn shared(&self) -> Arc<RuntimeRegistry> {
            Arc::clone(&self.registry)
        }
    }

    impl Drop for TestRegistry {
        fn drop(&mut self) {
            for snapshot in self.registry.inspect(None).unwrap_or_default() {
                let _ = self
                    .registry
                    .lifecycle(&snapshot.run_id, crate::protocol::LifecycleOperation::Stop);
            }
            let _ = fs::remove_dir_all(&self.state);
        }
    }

    fn registry() -> TestRegistry {
        let state = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-registry-test-{}-{}",
                std::process::id(),
                SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        let registry = RuntimeRegistry::new(&state, 64).expect("test registry");
        TestRegistry {
            registry: Arc::new(registry),
            state,
        }
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

    /// How long a subscribed exchange is allowed to stream before the loop returns. Production
    /// passes `None` here; a test cannot, because `exchange` waits for the server side to close.
    const TEST_STREAM_WINDOW: Duration = Duration::from_millis(400);

    fn exchange(lines: &[&str], runtime: &RuntimeRegistry) -> Vec<Response> {
        exchange_within(lines, runtime, TEST_STREAM_WINDOW)
    }

    fn exchange_within(
        lines: &[&str],
        runtime: &RuntimeRegistry,
        window: Duration,
    ) -> Vec<Response> {
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        std::thread::scope(|scope| {
            scope.spawn(move || {
                handle_connection_with_timeout(
                    &mut server,
                    runtime,
                    ReadTimeouts::PRODUCTION,
                    Some(Instant::now() + window),
                )
                .ok();
                // Dropping the server end is what gives the client its EOF; without it the
                // collecting reader below would wait out its own read timeout instead.
                drop(server);
            });
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
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
                    r#"{"type":"hello","version":7,"future":true}"#,
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
                        ReadTimeouts {
                            idle: Duration::from_millis(25),
                            in_flight: Duration::from_millis(25),
                        },
                        None
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

    /// The bound the old conflated timeout applied to *every* read, idle ones included. The
    /// idle test below has to exceed it by a margin no scheduling hiccup could explain away,
    /// or it would pass without ever having idled.
    const OLD_CONFLATED_TIMEOUT: Duration = Duration::from_secs(5);

    fn send_line(stream: &mut UnixStream, line: &str) {
        stream.write_all(line.as_bytes()).expect("write request");
        stream.write_all(b"\n").expect("write newline");
    }

    fn next_response(reader: &mut impl BufRead) -> Response {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).expect("read response") > 0,
            "daemon closed the connection instead of answering"
        );
        serde_json::from_str(&line).expect("response")
    }

    #[test]
    fn an_idle_connection_outlives_the_old_deadline_and_still_serves_the_next_request() {
        // The shipped defect: a dashboard that reads output or simply thinks for five seconds
        // had its request connection closed underneath it, and that connection has no
        // reconnect path. Production timeouts are used deliberately — the point is that the
        // real configuration tolerates a real human pause.
        let runtime = registry();
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let idle = OLD_CONFLATED_TIMEOUT + Duration::from_secs(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                handle_connection_with_timeout(
                    &mut server,
                    &runtime,
                    ReadTimeouts::PRODUCTION,
                    None,
                )
                .expect("an idle client is not a protocol fault");
            });
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let mut reader = BufReader::new(client.try_clone().expect("clone client"));
            send_line(&mut client, &hello());
            assert!(matches!(
                next_response(&mut reader),
                Response::Hello {
                    version: PROTOCOL_VERSION
                }
            ));

            let start = Instant::now();
            thread::sleep(idle);
            send_line(&mut client, r#"{"type":"inspect"}"#);
            // Any unsolicited `RequestTimeout` written during the pause would be read here
            // ahead of this reply, so a `Snapshots` answer also proves nothing was sent.
            let response = next_response(&mut reader);
            let idled = start.elapsed();
            assert!(
                matches!(response, Response::Snapshots { .. }),
                "a connection idle for {idled:?} must still serve requests, got {response:?}"
            );
            assert!(
                idled > OLD_CONFLATED_TIMEOUT,
                "the pause was only {idled:?}, which never reached the old {OLD_CONFLATED_TIMEOUT:?} bound"
            );
            client.shutdown(Shutdown::Write).expect("finish requests");
        });
    }

    #[test]
    fn a_half_sent_request_is_still_timed_out_however_long_idling_is_allowed() {
        // The protection the old timeout really provided: a message that starts and never
        // finishes must not pin the connection, and lengthening the *idle* bound must not
        // lengthen this one.
        let runtime = registry();
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let timeouts = ReadTimeouts {
            idle: Duration::from_secs(600),
            in_flight: Duration::from_millis(50),
        };
        std::thread::scope(|scope| {
            scope.spawn(|| {
                assert_eq!(
                    handle_connection_with_timeout(&mut server, &runtime, timeouts, None),
                    Err("request timed out".into())
                );
            });
            // Ten minutes of idle tolerance against two seconds of patience here: only the
            // in-flight bound can produce an answer inside this test.
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            let mut reader = BufReader::new(client.try_clone().expect("clone client"));
            send_line(&mut client, &hello());
            assert!(matches!(
                next_response(&mut reader),
                Response::Hello {
                    version: PROTOCOL_VERSION
                }
            ));
            client
                .write_all(br#"{"type":"inspect""#)
                .expect("partial request");
            assert!(matches!(
                next_response(&mut reader),
                Response::Error {
                    code: ErrorCode::RequestTimeout,
                    ..
                }
            ));
        });
    }

    #[test]
    fn a_client_that_closes_releases_its_admission_slot_without_waiting_for_a_deadline() {
        // Nothing about the long idle bound may delay reclamation of a connection whose peer
        // has gone: a closed socket reads as EOF, which ends the loop on its own.
        let runtime = registry();
        let admission = Arc::new(ClientAdmission::new(2));
        let permit = admission.try_acquire().expect("permit");
        assert_eq!(admission.active.load(Ordering::Acquire), 1);
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let shared = runtime.shared();
        let handler = std::thread::spawn(move || {
            let _permit = permit;
            handle_connection_with_timeout(
                &mut server,
                &shared,
                ReadTimeouts {
                    idle: Duration::from_secs(600),
                    in_flight: Duration::from_secs(600),
                },
                None,
            )
        });
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut reader = BufReader::new(client.try_clone().expect("clone client"));
        send_line(&mut client, &hello());
        assert!(matches!(
            next_response(&mut reader),
            Response::Hello {
                version: PROTOCOL_VERSION
            }
        ));
        drop(reader);
        drop(client);

        let start = Instant::now();
        assert_eq!(handler.join().expect("handler thread"), Ok(()));
        let reclaimed = start.elapsed();
        assert!(
            reclaimed < Duration::from_secs(5),
            "a departed client waited {reclaimed:?} to be noticed, so a deadline reclaimed it rather than its EOF"
        );
        assert_eq!(
            admission.active.load(Ordering::Acquire),
            0,
            "the handler's permit must be released when the connection ends"
        );
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
        let hello = r#"{"type":"hello","version":7}"#;
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
        let runtime = registry();
        let server_runtime = runtime.shared();
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
        let requests = [r#"{"type":"hello","version":7}"#, r#"{"type":"inspect"}"#];
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

    fn collect_events(responses: &[Response]) -> Vec<Event> {
        responses
            .iter()
            .filter_map(|response| match response {
                Response::Stream { event } => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    fn hello() -> String {
        serde_json::to_string(&Request::Hello(HelloRequest { version: 7 })).unwrap()
    }

    fn create_workspace(runtime: &RuntimeRegistry) {
        runtime
            .workspace(crate::protocol::WorkspaceRequest::Create {
                workspace_id: "w1".into(),
                name: "Daily".into(),
                pane_id: "p1".into(),
            })
            .expect("create workspace");
    }

    #[test]
    fn subscribe_streams_an_attach_snapshot_then_deltas() {
        let runtime = registry();
        create_workspace(&runtime);
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::Subscribe(SubscribeRequest {})).unwrap(),
            ],
            &runtime,
        );
        let events = collect_events(&responses);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::PaneAttached { .. })),
            "first frame for a bound pane must be a full attach snapshot"
        );
        let screen_frames: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, Event::PaneAttached { .. } | Event::PaneDelta { .. }))
            .collect();
        assert!(
            matches!(screen_frames[0], Event::PaneAttached { .. }),
            "the attach snapshot must precede any delta"
        );
        assert!(
            screen_frames[1..]
                .iter()
                .all(|event| matches!(event, Event::PaneDelta { .. })),
            "every frame after the attach snapshot must be a delta"
        );
        let revisions: Vec<u64> = screen_frames
            .iter()
            .map(|event| match event {
                Event::PaneAttached { revision, .. } | Event::PaneDelta { revision, .. } => {
                    *revision
                }
                other => panic!("unexpected frame {other:?}"),
            })
            .collect();
        assert!(
            revisions.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "revisions must be gapless and monotonic per run: {revisions:?}"
        );
    }

    #[test]
    fn an_unchanged_pane_costs_the_subscriber_nothing() {
        // With no runs at all the loop has nothing to diff, so a subscriber that polls for the
        // whole window must still receive zero bytes. This is the push model's whole argument.
        let runtime = registry();
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::Subscribe(SubscribeRequest {})).unwrap(),
            ],
            &runtime,
        );
        assert_eq!(
            collect_events(&responses),
            vec![],
            "an idle daemon must push nothing"
        );

        // With a live pane the shell writes a prompt and then falls silent. A polling server
        // would emit one frame per tick; the push server must emit far fewer than there were
        // ticks, which is only possible if unchanged ticks produced no event at all.
        let runtime = registry();
        create_workspace(&runtime);
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::Subscribe(SubscribeRequest {})).unwrap(),
            ],
            &runtime,
        );
        let ticks = TEST_STREAM_WINDOW.as_millis() / STREAM_POLL_INTERVAL.as_millis();
        let frames = collect_events(&responses).len() as u128;
        assert!(
            frames * 2 < ticks,
            "expected most of the {ticks} ticks to be silent, got {frames} frames"
        );
    }

    #[test]
    fn only_a_real_agent_state_change_is_announced() {
        let runtime = registry();
        create_workspace(&runtime);
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::Subscribe(SubscribeRequest {})).unwrap(),
            ],
            &runtime,
        );
        let announcements = collect_events(&responses)
            .into_iter()
            .filter(|event| matches!(event, Event::AgentStateChanged { .. }))
            .count();
        assert!(
            announcements <= 1,
            "a pane whose agent state never changes must be announced at most once, got {announcements}"
        );
    }

    #[test]
    fn an_exited_shell_is_announced_even_though_its_screen_stops_changing() {
        let runtime = registry();
        create_workspace(&runtime);
        let shared = runtime.shared();
        // Typed from another thread so the exit lands while this subscriber is attached. A
        // shell that had already died before the stream opened would be reported by the very
        // first frame and would prove nothing about noticing a *change*.
        let typist = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            let _ = shared.pane_input("w1", "p1", b"exit\n");
        });
        let responses = exchange_within(
            &[&hello(), &subscribe_line()],
            &runtime,
            Duration::from_millis(2500),
        );
        typist.join().expect("typist thread");

        let announced: Vec<ProcessState> = collect_events(&responses)
            .into_iter()
            .filter_map(|event| match event {
                Event::PaneState { state, .. } => Some(state),
                _ => None,
            })
            .collect();
        assert!(
            announced
                .iter()
                .any(|state| matches!(state, ProcessState::Exited { .. })),
            "a shell that exits must reach the subscriber: its screen stops changing and a \
             plain shell never changes agent state, so nothing else carries the news; got \
             {announced:?}"
        );
        assert_eq!(
            announced
                .iter()
                .filter(|state| matches!(state, ProcessState::Running))
                .count(),
            1,
            "process state is change-gated, so Running must be announced once, not per tick: \
             {announced:?}"
        );
    }

    /// Backstop only: the subscribers below stop as soon as the daemon has demonstrably gone
    /// quiet, so this bound is never reached unless something has genuinely wedged.
    const CONVERGENCE_BACKSTOP: Duration = Duration::from_millis(5000);
    /// How long a subscriber must receive nothing before it calls the daemon quiescent. The
    /// comparison against the live screen is taken immediately after that, so this is the
    /// window in which a late write could still race the assertion.
    const QUIET: Duration = Duration::from_millis(400);

    fn subscribe_line() -> String {
        serde_json::to_string(&Request::Subscribe(SubscribeRequest {})).unwrap()
    }

    /// Reads pushed frames until the daemon has been silent for `QUIET` *and* the test has
    /// signalled that it is done making changes. Stopping on proven silence rather than on a
    /// clock is what lets the assertion compare against the live screen without racing it:
    /// anything the daemon wrote, this subscriber has already received.
    fn drain_until_quiet(client: UnixStream, ready: &std::sync::atomic::AtomicBool) -> Vec<Event> {
        client.set_read_timeout(Some(QUIET)).expect("read timeout");
        let mut reader = BufReader::new(client);
        let mut responses = Vec::new();
        // Held across iterations: a read that times out part-way through a frame must resume
        // rather than discard the bytes it already has.
        let mut pending = String::new();
        loop {
            match reader.read_line(&mut pending) {
                Ok(0) => break,
                Ok(_) => {
                    if pending.ends_with('\n') {
                        let response: Response = serde_json::from_str(&pending).expect("response");
                        responses.push(response);
                        pending.clear();
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    if ready.load(Ordering::Acquire) && pending.is_empty() {
                        break;
                    }
                }
                Err(error) => panic!("subscriber read failed: {error}"),
            }
        }
        collect_events(&responses)
    }

    /// Replays what one subscriber received into a terminal of its own, exactly as a client
    /// must: size the parser from the attach frame, then feed every delta.
    fn replay(events: &[Event], run_id: &str) -> crate::terminal::VtTerminal {
        let mut screen: Option<crate::terminal::VtTerminal> = None;
        for event in events {
            match event {
                Event::PaneAttached {
                    run_id: id,
                    rows,
                    cols,
                    screen: bytes,
                    ..
                } if id == run_id => {
                    // The whole point of the geometry fields: this client never saw the
                    // resize request and has no other source for the new size.
                    let mut fresh = crate::terminal::VtTerminal::new(*rows, *cols, 0);
                    fresh.feed(&STANDARD.decode(bytes).expect("attach screen is base64"));
                    screen = Some(fresh);
                }
                Event::PaneDelta {
                    run_id: id, bytes, ..
                } if id == run_id => {
                    let screen = screen.as_mut().expect("a delta before any attach frame");
                    screen.feed(&STANDARD.decode(bytes).expect("delta is base64"));
                }
                _ => {}
            }
        }
        screen.expect("subscriber never received an attach frame")
    }

    fn screen_revisions(events: &[Event]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::PaneAttached { revision, .. } | Event::PaneDelta { revision, .. } => {
                    Some(*revision)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_subscriber_converges_across_a_resize_it_did_not_originate() {
        let runtime = registry();
        create_workspace(&runtime);
        let run_id = runtime
            .inspect(None)
            .expect("inspect")
            .into_iter()
            .find(|run| run.pane_id == "p1")
            .expect("bound run")
            .run_id;
        let deadline = Instant::now() + CONVERGENCE_BACKSTOP;
        let ready = std::sync::atomic::AtomicBool::new(false);

        let mut clients = Vec::new();
        for _ in 0..2 {
            let (client, mut server) = UnixStream::pair().expect("socket pair");
            let shared = runtime.shared();
            // Detached: a quiet stream never writes, so the handler cannot notice the client
            // leaving and would otherwise hold the test open until its backstop expires.
            std::thread::spawn(move || {
                handle_connection_with_timeout(
                    &mut server,
                    &shared,
                    ReadTimeouts::PRODUCTION,
                    Some(deadline),
                )
                .ok();
            });
            clients.push(client);
        }

        let registry_ref: &RuntimeRegistry = &runtime;
        let (first, second) = std::thread::scope(|scope| {
            let mut readers = Vec::new();
            for mut client in clients {
                let ready = &ready;
                readers.push(scope.spawn(move || {
                    for line in [hello(), subscribe_line()] {
                        client.write_all(line.as_bytes()).expect("write request");
                        client.write_all(b"\n").expect("write newline");
                    }
                    client.shutdown(Shutdown::Write).expect("finish requests");
                    drain_until_quiet(client, ready)
                }));
            }

            let live = || {
                registry_ref
                    .with_run_screen(&run_id, |screen| screen.state_bytes())
                    .expect("live screen")
            };
            let headroom = |what: &str| {
                assert!(
                    Instant::now() + QUIET + Duration::from_millis(600) < deadline,
                    "{what} did not happen inside the stream window"
                );
            };

            // Let both subscribers attach at the original geometry before anything changes.
            thread::sleep(Duration::from_millis(250));
            // The resize arrives on a third, ordinary request connection: neither subscriber
            // originated it, so neither learns the new size except from the stream itself.
            let resized = exchange(
                &[
                    &hello(),
                    &serde_json::to_string(&Request::PaneResize(PaneResizeRequest {
                        workspace_id: "w1".into(),
                        pane_id: "p1".into(),
                        rows: 40,
                        cols: 120,
                    }))
                    .unwrap(),
                ],
                registry_ref,
            );
            assert!(matches!(resized[1], Response::Ack));

            // Fill the screen past the *old* height. Without this the pane holds only a short
            // prompt, and vt100 elides trailing blank rows, so an undersized replay would
            // still produce byte-identical `state_formatted` output and the comparison below
            // would pass vacuously.
            let before = live();
            let typed = exchange(
                &[
                    &hello(),
                    &serde_json::to_string(&Request::PaneInput(PaneInputRequest {
                        workspace_id: "w1".into(),
                        pane_id: "p1".into(),
                        input: PaneInputRequest::encode(b"seq 1 50\n"),
                    }))
                    .unwrap(),
                ],
                registry_ref,
            );
            assert!(matches!(typed[1], Response::PaneInputAccepted { .. }));
            while live() == before {
                headroom("the shell never echoed the typed command");
                thread::sleep(Duration::from_millis(50));
            }
            headroom("the screen filled too late");

            // Nothing else will change the pane. Whatever the shell still emits — a late
            // asynchronous prompt segment, say — the subscribers keep receiving until they
            // have seen `QUIET` of silence, so they cannot be left behind the live screen.
            ready.store(true, Ordering::Release);
            let mut done = readers.into_iter();
            let first = done.next().unwrap().join().expect("first subscriber");
            let second = done.next().unwrap().join().expect("second subscriber");
            (first, second)
        });

        let expected = runtime
            .with_run_screen(&run_id, |screen| screen.state_bytes())
            .expect("live screen");
        for (label, events) in [("first", &first), ("second", &second)] {
            let attached: Vec<_> = events
                .iter()
                .filter(|event| matches!(event, Event::PaneAttached { .. }))
                .collect();
            assert_eq!(
                attached.len(),
                2,
                "{label} subscriber must be re-seeded by the resize, got {attached:?}"
            );
            let reseed = match attached[1] {
                Event::PaneAttached {
                    revision,
                    rows,
                    cols,
                    ..
                } => (*revision, (*rows, *cols)),
                other => panic!("expected an attach frame, got {other:?}"),
            };
            assert_eq!(
                reseed.1,
                (40, 120),
                "{label} subscriber's re-seed must announce the new geometry"
            );
            assert!(
                reseed.0 > 1,
                "{label} subscriber's re-seed restarted the numbering at {} instead of carrying it forward",
                reseed.0
            );
            let replayed = replay(events, &run_id);
            assert_eq!(
                replayed.size(),
                (40, 120),
                "{label} subscriber must size its parser from the attach frame alone"
            );
            assert_eq!(
                String::from_utf8_lossy(&replayed.state_bytes()),
                String::from_utf8_lossy(&expected),
                "{label} subscriber did not converge on the daemon's live screen"
            );
            let revisions = screen_revisions(events);
            assert!(
                revisions.windows(2).all(|pair| pair[1] == pair[0] + 1),
                "{label} subscriber saw a revision gap or reset across the resize: {revisions:?}"
            );
        }
    }

    #[test]
    fn resize_request_is_routed_to_the_registry() {
        let runtime = registry();
        create_workspace(&runtime);
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::PaneResize(PaneResizeRequest {
                    workspace_id: "w1".into(),
                    pane_id: "p1".into(),
                    rows: 40,
                    cols: 120,
                }))
                .unwrap(),
            ],
            &runtime,
        );
        assert!(!matches!(responses[1], Response::Error { .. }));
        assert!(matches!(responses[1], Response::Ack));
        let snapshot = runtime
            .inspect(None)
            .expect("inspect")
            .into_iter()
            .find(|run| run.pane_id == "p1")
            .expect("bound run");
        assert_eq!((snapshot.rows, snapshot.cols), (40, 120));
    }

    #[test]
    fn an_unresizable_pane_is_refused_rather_than_acknowledged() {
        let runtime = registry();
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::PaneResize(PaneResizeRequest {
                    workspace_id: "missing".into(),
                    pane_id: "p1".into(),
                    rows: 40,
                    cols: 120,
                }))
                .unwrap(),
            ],
            &runtime,
        );
        assert!(matches!(
            responses[1],
            Response::Error {
                code: ErrorCode::InvalidBinding,
                ..
            }
        ));
    }

    #[test]
    fn pane_input_is_base64_decoded_before_it_reaches_the_runtime() {
        let runtime = registry();
        create_workspace(&runtime);
        // An arrow key: the escape byte and a high byte, neither of which survives being
        // forwarded as raw UTF-8 text.
        let raw = [0x1b_u8, 0x5b, 0x41, 0xff];
        let encoded = PaneInputRequest::encode(&raw);
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::PaneInput(PaneInputRequest {
                    workspace_id: "w1".into(),
                    pane_id: "p1".into(),
                    input: encoded.clone(),
                }))
                .unwrap(),
            ],
            &runtime,
        );
        match &responses[1] {
            Response::PaneInputAccepted { bytes, .. } => assert_eq!(
                *bytes,
                raw.len(),
                "the runtime must receive the decoded bytes, not the {} base64 characters",
                encoded.len()
            ),
            other => panic!("expected pane input to be accepted, got {other:?}"),
        }
    }

    #[test]
    fn pane_input_that_is_not_base64_is_refused_not_forwarded() {
        let runtime = registry();
        create_workspace(&runtime);
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::PaneInput(PaneInputRequest {
                    workspace_id: "w1".into(),
                    pane_id: "p1".into(),
                    input: "not base64!!".into(),
                }))
                .unwrap(),
            ],
            &runtime,
        );
        assert!(matches!(
            responses[1],
            Response::Error {
                code: ErrorCode::InvalidBinding,
                ..
            }
        ));
    }

    #[test]
    fn version_six_clients_are_refused_with_an_actionable_message() {
        let runtime = registry();
        let responses = exchange(
            &[&serde_json::to_string(&Request::Hello(HelloRequest { version: 6 })).unwrap()],
            &runtime,
        );
        match &responses[0] {
            Response::Error { code, message } => {
                assert_eq!(*code, ErrorCode::ProtocolMismatch);
                assert!(message.contains("7"));
            }
            other => panic!("expected protocol mismatch, got {other:?}"),
        }
    }

    #[test]
    fn workspace_socket_lifecycle_is_end_to_end() {
        let runtime = registry();
        let responses = exchange(
            &[
                r#"{"type":"hello","version":7}"#,
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
