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
    terminal::{PaneOutput, ScreenSync},
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

/// Pushes screen frames to one subscriber until it disconnects.
///
/// Each run gets a `SubscriberView` recording how far into that pane's raw output this
/// subscriber has read. A frame carries the child's own bytes from that point on, so the
/// client's parser scrolls exactly as the daemon's did and accumulates the same history. The
/// earlier scheme sent `state_diff` repaints, which are cursor-addressed and therefore never
/// scroll: a client fed them could never hold a single row of scrollback, so the mouse wheel
/// had nothing to scroll into and copy mode had no history to search.
///
/// An idle pane writes no bytes, so it produces no frame at all — the entire point of pushing
/// instead of answering `Inspect` polls with every run's full scrollback.
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
            let scrollback_rows = u32::try_from(runtime.scrollback_rows()).unwrap_or(u32::MAX);
            let frame = runtime.with_run_output(&snapshot.run_id, |output| {
                match syncs.get_mut(&snapshot.run_id) {
                    Some(view) => view.next_delta(output),
                    // Falls through to a seed below, exactly as a view that has fallen behind
                    // the retained output does.
                    None => None,
                }
                .map_or_else(
                    || {
                        StreamFrame::Seed(Box::new(SubscriberView::seeded(
                            output,
                            snapshot.rows,
                            snapshot.cols,
                        )))
                    },
                    StreamFrame::Delta,
                )
            });
            // No live runtime to read: leave this run unattached so it still gets a full
            // snapshot rather than a delta if it becomes readable later.
            let Some(frame) = frame else { continue };
            match frame {
                StreamFrame::Delta(bytes) if bytes.is_empty() => {}
                StreamFrame::Delta(bytes) => {
                    let revision = revisions.get(&snapshot.run_id).copied().unwrap_or(0) + 1;
                    write_response(
                        stream,
                        &Response::Stream {
                            event: Event::PaneDelta {
                                run_id: snapshot.run_id.clone(),
                                revision,
                                bytes: STANDARD.encode(&bytes),
                            },
                        },
                    )?;
                    revisions.insert(snapshot.run_id.clone(), revision);
                }
                StreamFrame::Seed(seed) => {
                    let (view, bytes) = *seed;
                    let revision = revisions.get(&snapshot.run_id).copied().unwrap_or(0) + 1;
                    write_response(
                        stream,
                        &Response::Stream {
                            event: Event::PaneAttached {
                                run_id: snapshot.run_id.clone(),
                                revision,
                                rows: snapshot.rows,
                                cols: snapshot.cols,
                                scrollback_rows,
                                screen: STANDARD.encode(&bytes),
                            },
                        },
                    )?;
                    // Recorded only once the seed is on the wire, so a failed write cannot
                    // leave this loop believing the subscriber is attached.
                    syncs.insert(snapshot.run_id.clone(), view);
                    revisions.insert(snapshot.run_id.clone(), revision);
                }
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

/// What one poll owes one subscriber for one run.
enum StreamFrame {
    /// The child's own bytes since this subscriber's last frame, with any repaint needed to
    /// erase drift appended. Empty when the pane has been silent, which is the common case.
    Delta(Vec<u8>),
    /// A full snapshot and the view that replaces this subscriber's, for a run it has never
    /// seen, one whose geometry changed, or one whose output it has fallen behind.
    /// Boxed only to keep the common `Delta` frame small: a view carries two parsers.
    Seed(Box<(SubscriberView, Vec<u8>)>),
}

/// How far into one run's raw output this subscriber has read, a replica of what it should
/// now be showing, and the geometry the view was built at. Revisions live outside this so
/// dropping it to re-seed cannot reset them.
struct SubscriberView {
    sync: ScreenSync,
    size: (u16, u16),
    /// Sequence of the first byte of this pane's output this subscriber has not been sent.
    offset: u64,
    /// Which byte stream that sequence belongs to. A restarted run keeps its id but gets a
    /// fresh terminal whose sequence starts over, so without this a stale offset would be
    /// served bytes from the middle of the replacement.
    epoch: u64,
}

impl SubscriberView {
    /// A snapshot of `output`'s screen and the view a subscriber sent it will then have.
    ///
    /// The snapshot is a repaint, which restores the visible grid but says nothing about
    /// which buffer that grid is. A pane already in the alternate screen must therefore say
    /// so first, or the client would paint a full-screen program's window onto its primary
    /// buffer — scrolling the user's real history away and leaving the client on the wrong
    /// buffer when the program exits.
    ///
    /// There is deliberately no matching `\e[?1049l` for a pane on the primary screen, and
    /// that is only safe because `Dashboard::apply_event` rebuilds the client's parser on
    /// every `PaneAttached` (`src/dashboard.rs`), so a seed always lands in a fresh primary
    /// buffer. Reusing an existing parser across a re-attach — to preserve its accumulated
    /// history, say — would strand a client that was in the alternate screen when the pane
    /// left it. Any such change must send the leave sequence from here.
    fn seeded(output: &PaneOutput, rows: u16, cols: u16) -> (Self, Vec<u8>) {
        let mut bytes = Vec::new();
        if output.screen().alternate_screen() {
            bytes.extend_from_slice(b"\x1b[?1049h");
        }
        bytes.extend_from_slice(&output.screen().state_bytes());
        let mut view = Self {
            sync: ScreenSync::new(rows, cols),
            size: (rows, cols),
            offset: output.log().end(),
            epoch: output.log().epoch(),
        };
        view.sync.apply(&bytes);
        (view, bytes)
    }

    /// The bytes owed to this subscriber, or `None` if it has fallen further behind than the
    /// pane retains and must be re-seeded instead.
    ///
    /// The view advances here rather than after the write, because the correction below has to
    /// be computed against the screen those exact bytes reach. Reading the screen again after
    /// the write would diff against a screen that had moved on, and the client would then be
    /// repainted with output it was about to be sent as bytes — printing it twice. A failed
    /// write ends the connection and discards every view with it, so nothing survives to be
    /// left inconsistent by advancing early.
    fn next_delta(&mut self, output: &PaneOutput) -> Option<Vec<u8>> {
        if output.log().epoch() != self.epoch {
            return None;
        }
        let mut pending = output.log().since(self.offset)?;
        if pending.is_empty() {
            return Some(pending);
        }
        self.offset += pending.len() as u64;
        self.sync.apply(&pending);
        // Replaying the child's bytes reproduces the daemon's screen for everything the
        // subscriber witnessed, but not for state it never saw — a scroll region set before it
        // attached, say. This erases whatever difference is left, and is empty in the ordinary
        // case. It is cursor-addressed, so it repaints without disturbing the history the
        // bytes above just built.
        let correction = self.sync.delta_from(output.screen());
        if !correction.is_empty() {
            self.sync.apply(&correction);
            pending.extend_from_slice(&correction);
        }
        Some(pending)
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
                    r#"{"type":"hello","version":8,"future":true}"#,
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
        let hello = r#"{"type":"hello","version":8}"#;
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
        let requests = [r#"{"type":"hello","version":8}"#, r#"{"type":"inspect"}"#];
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
        serde_json::to_string(&Request::Hello(HelloRequest {
            version: PROTOCOL_VERSION,
        }))
        .unwrap()
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
        let deadline = Instant::now() + CONVERGENCE_BACKSTOP;
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let shared = runtime.shared();
        // Detached: a quiet stream never writes, so the handler cannot notice this client
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
        for line in [hello(), subscribe_line()] {
            client.write_all(line.as_bytes()).expect("write request");
            client.write_all(b"\n").expect("write newline");
        }
        client.shutdown(Shutdown::Write).expect("finish requests");
        client
            .set_read_timeout(Some(DRAIN_POLL))
            .expect("read timeout");
        let mut reader = BufReader::new(client);
        let mut pending = String::new();
        let mut events: Vec<Event> = Vec::new();

        let running = |events: &[Event]| {
            events.iter().any(|event| {
                matches!(
                    event,
                    Event::PaneState {
                        state: ProcessState::Running,
                        ..
                    }
                )
            })
        };
        // The exit must land while this subscriber is attached *and* has already been told the
        // shell is running: a shell that had died before the stream opened would be reported by
        // the very first frame and would prove nothing about noticing a *change*. Waiting for
        // that announcement rather than sleeping a fixed 250 ms is what makes the ordering
        // certain — under load the old sleep could elapse before the subscriber had attached at
        // all, and the fixed 2.5 s window after it could close before the shell had died.
        read_events_until(&mut reader, &mut pending, &mut events, deadline, running);
        assert!(
            running(&events),
            "the subscriber was never told the shell was running, so nothing here could \
             demonstrate noticing a change: {events:?}"
        );
        runtime
            .pane_input("w1", "p1", b"exit\n")
            .expect("type exit into the pane");
        read_events_until(&mut reader, &mut pending, &mut events, deadline, |events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    Event::PaneState {
                        state: ProcessState::Exited { .. },
                        ..
                    }
                )
            })
        });

        let announced: Vec<ProcessState> = events
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

    /// Backstop only: the subscribers below stop the moment their replica agrees with the
    /// daemon's live screen, so this bound is reached only when convergence never happens —
    /// which is a genuine failure, not a slow machine. It is generous for that reason: a
    /// subscriber that has converged returns immediately regardless of how large this is.
    const CONVERGENCE_BACKSTOP: Duration = Duration::from_millis(15_000);
    /// How long a subscriber waits for a frame before re-checking whether it has converged.
    /// Only a polling interval: nothing is concluded from silence.
    const DRAIN_POLL: Duration = Duration::from_millis(100);

    fn subscribe_line() -> String {
        serde_json::to_string(&Request::Subscribe(SubscribeRequest {})).unwrap()
    }

    /// Reads pushed frames until this subscriber's replay of them matches the daemon's live
    /// screen, once the test has signalled that it is done making changes.
    ///
    /// Stops on the property under test rather than on a proxy for it. Silence is not a sound
    /// proxy: a real `$SHELL` writes on its own schedule, and oh-my-zsh repaints its prompt
    /// when an asynchronous `git status` returns, which can land after any fixed quiet window.
    /// Measured on this suite: a subscriber that drained on 400 ms of quiet held `➜  dock `
    /// while the daemon had already repainted `➜  dock git:(slice/a1-copy-mode) ✗`, and the
    /// convergence assertion then reported a divergence the daemon was about to close by
    /// itself. Waiting for agreement instead cannot stop early *or* wait longer than it must.
    ///
    /// `ready` still gates when checking begins, because a freshly attached subscriber agrees
    /// with the live screen trivially — before the resize or the output the test is about to
    /// produce has happened at all.
    ///
    /// Returns the frames *and the exact live screen they were found to match*. The caller must
    /// assert against that returned screen and never re-read the daemon afterwards: the pane
    /// keeps painting — oh-my-zsh emits further prompt segments — so a fresh read taken after
    /// this returns describes a later screen than the one convergence was decided on, and the
    /// comparison would then report a divergence that is only the gap between the two reads.
    fn drain_until_converged(
        client: UnixStream,
        run_id: &str,
        ready: &std::sync::atomic::AtomicBool,
        live: impl Fn() -> Vec<u8>,
        deadline: Instant,
    ) -> (Vec<Event>, Vec<u8>) {
        client
            .set_read_timeout(Some(DRAIN_POLL))
            .expect("read timeout");
        let mut reader = BufReader::new(client);
        let mut responses = Vec::new();
        // Held across iterations: a read that times out part-way through a frame must resume
        // rather than discard the bytes it already has.
        let mut pending = String::new();
        // Yields the live screen that was matched, so the caller asserts against the very
        // bytes this comparison used rather than against a later read of a still-painting pane.
        let converged = |responses: &[Response]| -> Option<Vec<u8>> {
            let events = collect_events(responses);
            if !events
                .iter()
                .any(|event| matches!(event, Event::PaneAttached { .. }))
            {
                return None;
            }
            let matched = live();
            (replay(&events, run_id).state_bytes() == matched).then_some(matched)
        };
        let mut matched = None;
        loop {
            if Instant::now() >= deadline {
                break;
            }
            match reader.read_line(&mut pending) {
                Ok(0) => break,
                Ok(_) => {
                    if pending.ends_with('\n') {
                        let response: Response = serde_json::from_str(&pending).expect("response");
                        responses.push(response);
                        pending.clear();
                        if ready.load(Ordering::Acquire)
                            && let Some(screen) = converged(&responses)
                        {
                            matched = Some(screen);
                            break;
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    if ready.load(Ordering::Acquire)
                        && pending.is_empty()
                        && let Some(screen) = converged(&responses)
                    {
                        matched = Some(screen);
                        break;
                    }
                }
                Err(error) => panic!("subscriber read failed: {error}"),
            }
        }
        // Never converged: hand back a final read so the caller's assertion can show the two
        // screens side by side. That assertion is going to fail, which is the correct outcome.
        (collect_events(&responses), matched.unwrap_or_else(live))
    }

    /// Reads pushed frames until `stop` is satisfied, or the backstop expires.
    ///
    /// The stop condition is the property the caller is waiting for, never elapsed time. A real
    /// `$SHELL` under load can take longer to start, to echo, or to die than any fixed window,
    /// and a window that closes early makes the assertion after it describe the test's
    /// impatience rather than the daemon's behaviour.
    ///
    /// `pending` is threaded through so consecutive waits share one buffer: a read that times
    /// out part-way through a frame must resume, not discard the bytes it already has.
    fn read_events_until(
        reader: &mut BufReader<UnixStream>,
        pending: &mut String,
        events: &mut Vec<Event>,
        deadline: Instant,
        stop: impl Fn(&[Event]) -> bool,
    ) {
        loop {
            if stop(events) || Instant::now() >= deadline {
                return;
            }
            match reader.read_line(pending) {
                Ok(0) => return,
                Ok(_) => {
                    if pending.ends_with('\n') {
                        if let Response::Stream { event } =
                            serde_json::from_str(pending).expect("response")
                        {
                            events.push(event);
                        }
                        pending.clear();
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => panic!("subscriber read failed: {error}"),
            }
        }
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
                    scrollback_rows,
                    screen: bytes,
                    ..
                } if id == run_id => {
                    // The whole point of the geometry fields: this client never saw the
                    // resize request and has no other source for the new size — nor for how
                    // much history the daemon keeps, which decides how far back it can scroll.
                    let mut fresh =
                        crate::terminal::VtTerminal::new(*rows, *cols, *scrollback_rows as usize);
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

    /// The defect this whole path exists to prevent: a client fed cursor-addressed repaints
    /// renders correctly and still has no history, because addressing a cell never scrolls a
    /// row into scrollback. So this drives real output from a real child through the real
    /// stream and then scrolls the replica the stream built. Filling a replica with `feed()`
    /// would exercise the client's own scroll path and prove nothing about the transport.
    #[test]
    fn a_subscriber_scrolls_back_through_the_history_the_stream_itself_built() {
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
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let shared = runtime.shared();
        std::thread::spawn(move || {
            handle_connection_with_timeout(
                &mut server,
                &shared,
                ReadTimeouts::PRODUCTION,
                Some(deadline),
            )
            .ok();
        });

        let registry_ref: &RuntimeRegistry = &runtime;
        // `&str` rather than `&String` so a `move` reader closure copies the reference instead
        // of moving the owned value the outer test still needs.
        let run_id_ref: &str = &run_id;
        // Captures only shared references, so each reader thread gets its own copy.
        let live = || {
            registry_ref
                .with_run_screen(run_id_ref, |screen| screen.state_bytes())
                .expect("live screen")
        };
        // `expected` is the screen the subscriber was found to agree with, captured inside the
        // drain. Re-reading the daemon here instead would describe a later screen — the shell
        // keeps painting its prompt — and the assertion would report that gap as a divergence.
        let (events, expected) = std::thread::scope(|scope| {
            let ready_ref = &ready;
            let reader = scope.spawn(move || {
                for line in [hello(), subscribe_line()] {
                    client.write_all(line.as_bytes()).expect("write request");
                    client.write_all(b"\n").expect("write newline");
                }
                client.shutdown(Shutdown::Write).expect("finish requests");
                drain_until_converged(client, run_id_ref, ready_ref, live, deadline)
            });
            // Let the subscriber attach before any of the output below exists, so every line
            // it can scroll back to reached it as a delta rather than in the attach snapshot.
            thread::sleep(Duration::from_millis(250));
            let typed = exchange(
                &[
                    &hello(),
                    &serde_json::to_string(&Request::PaneInput(PaneInputRequest {
                        workspace_id: "w1".into(),
                        pane_id: "p1".into(),
                        // The trailing marker is quoted so the *echoed command line* reads
                        // `DONE""MARK` while only the shell's *output* reads `DONEMARK`. A
                        // gate that could match the echo fires before `seq` has produced
                        // anything, and the assertions below then run against a pane holding
                        // only a prompt — which fails with the same message the original
                        // defect produced, so the flake reads as a regression every time.
                        input: PaneInputRequest::encode(b"seq 1 200; echo DONE\"\"MARK\n"),
                    }))
                    .unwrap(),
                ],
                registry_ref,
            );
            assert!(matches!(typed[1], Response::PaneInputAccepted { .. }));
            while !registry_ref
                .with_run_screen(&run_id, |screen| screen.text_tail(2))
                .expect("live screen")
                .contains("DONEMARK")
            {
                assert!(
                    Instant::now() + Duration::from_secs(1) < deadline,
                    "the shell never produced the requested output"
                );
                thread::sleep(Duration::from_millis(50));
            }
            ready_ref.store(true, Ordering::Release);
            reader.join().expect("subscriber")
        });

        let attached = events
            .iter()
            .find_map(|event| match event {
                Event::PaneAttached {
                    scrollback_rows, ..
                } => Some(*scrollback_rows),
                _ => None,
            })
            .expect("an attach frame");
        assert_eq!(
            attached as usize,
            runtime.scrollback_rows(),
            "the attach frame must carry the daemon's real retention, not a client-side guess"
        );

        let mut replayed = replay(&events, &run_id);
        assert_eq!(
            String::from_utf8_lossy(&replayed.state_bytes()),
            String::from_utf8_lossy(&expected),
            "the subscriber did not converge on the daemon's live screen"
        );

        let visible = |terminal: &crate::terminal::VtTerminal| {
            let (rows, _) = terminal.size();
            (0..rows)
                .map(|row| terminal.visible_row(row))
                .collect::<Vec<_>>()
        };
        // The lowest number `seq` printed that is still on screen. Comparing these rather than
        // fixed line numbers keeps the assertion independent of the pane's height.
        let earliest = |view: &[String]| {
            view.iter()
                .filter_map(|row| row.trim().parse::<u64>().ok())
                .min()
                .expect("the pane is showing numbered output")
        };
        let live_view = visible(&replayed);
        assert!(!replayed.is_scrolled());
        replayed.scroll_by(30);
        assert!(
            replayed.is_scrolled(),
            "the replica the stream built retained no history at all, so the wheel does nothing"
        );
        let scrolled_view = visible(&replayed);
        assert!(
            earliest(&scrolled_view) < earliest(&live_view),
            "scrolling back showed nothing older: {:?} then {:?}",
            earliest(&live_view),
            earliest(&scrolled_view)
        );
        replayed.scroll_to_live();
        assert_eq!(
            visible(&replayed),
            live_view,
            "returning to the bottom must resume following live output"
        );
    }

    /// A subscriber slow enough to fall past the retained window must be re-seeded. Serving it
    /// whatever bytes survive would skip the rest, and a mirror missing a run of bytes looks
    /// exactly like a rendering bug with nothing to attribute it to.
    #[test]
    fn a_subscriber_that_falls_behind_the_retained_output_is_re_seeded_rather_than_skipped() {
        let mut output = PaneOutput::new(5, 20, 100, 16);
        output.feed(b"first\r\n");
        let (mut view, seed) = SubscriberView::seeded(&output, 5, 20);
        let mut client = crate::terminal::VtTerminal::new(5, 20, 100);
        client.feed(&seed);
        assert_eq!(client.state_bytes(), output.screen().state_bytes());

        // Keeping up: the child's own bytes are forwarded and the replica tracks them.
        output.feed(b"second\r\n");
        let delta = view.next_delta(&output).expect("still retained");
        client.feed(&delta);
        assert_eq!(client.state_bytes(), output.screen().state_bytes());
        assert_eq!(
            view.next_delta(&output).as_deref(),
            Some(b"".as_slice()),
            "a silent pane owes a caught-up subscriber nothing"
        );

        // Falling behind: far more arrives than the pane retains undelivered.
        for _ in 0..10 {
            output.feed(b"a longer burst\r\n");
        }
        assert!(
            view.next_delta(&output).is_none(),
            "falling behind must be reported, not silently partially served"
        );
        let (mut recovered, reseed) = SubscriberView::seeded(&output, 5, 20);
        client.feed(&reseed);
        assert_eq!(
            client.state_bytes(),
            output.screen().state_bytes(),
            "the re-seed must put the subscriber back on the daemon's screen"
        );

        // A restart keeps the run id but replaces its terminal, so the sequence starts over.
        // A view holding an offset from the old stream must not be served bytes from the
        // middle of the new one — which is exactly what a bare offset comparison would do.
        let mut restarted = PaneOutput::new(5, 20, 100, 4096);
        restarted.feed(b"a replacement terminal wrote this\r\n");
        assert!(
            recovered.next_delta(&restarted).is_none(),
            "a restarted run's output must not be read as a continuation of the old run's"
        );
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
        // `&str` rather than `&String` so a `move` reader closure copies the reference instead
        // of moving the owned value the outer test still needs.
        let run_id_ref: &str = &run_id;
        // Captures only shared references, so each reader thread gets its own copy.
        let live = || {
            registry_ref
                .with_run_screen(run_id_ref, |screen| screen.state_bytes())
                .expect("live screen")
        };
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
                    drain_until_converged(client, run_id_ref, ready, live, deadline)
                }));
            }

            let headroom = |what: &str| {
                assert!(
                    Instant::now() + Duration::from_secs(1) < deadline,
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
            //
            // The readiness gate below waits for the marker's *output*, not for the screen to
            // change at all. "Changed at all" is satisfied by the shell echoing the command
            // line, which happens within a few tens of milliseconds — long before `seq` has
            // printed anything. `ready` would then be set while the pane was still silent for
            // reasons of its own, the drain would take that silence for the daemon
            // having finished, and both subscribers would be asserted against a screen the
            // daemon had not sent them yet. Measured: the gate fired at +330 ms and the drain
            // stopped 400 ms later holding six frames, with four and a half seconds of
            // backstop still unused. Quoting the marker keeps the echo (`DONE""MARK`) from
            // matching what only the output (`DONEMARK`) contains.
            let typed = exchange(
                &[
                    &hello(),
                    &serde_json::to_string(&Request::PaneInput(PaneInputRequest {
                        workspace_id: "w1".into(),
                        pane_id: "p1".into(),
                        input: PaneInputRequest::encode(b"seq 1 50; echo DONE\"\"MARK\n"),
                    }))
                    .unwrap(),
                ],
                registry_ref,
            );
            assert!(matches!(typed[1], Response::PaneInputAccepted { .. }));
            while !registry_ref
                .with_run_screen(&run_id, |screen| screen.text_tail(2))
                .expect("live screen")
                .contains("DONEMARK")
            {
                headroom("the shell never produced the requested output");
                thread::sleep(Duration::from_millis(50));
            }
            headroom("the screen filled too late");

            // Nothing else will change the pane on the test's account. Whatever the shell
            // still emits on its own — a late asynchronous prompt segment, say — the
            // subscribers keep receiving until their replica actually matches the live
            // screen, so they cannot be left behind it.
            ready.store(true, Ordering::Release);
            let mut done = readers.into_iter();
            let first = done.next().unwrap().join().expect("first subscriber");
            let second = done.next().unwrap().join().expect("second subscriber");
            (first, second)
        });

        // Each subscriber is asserted against the screen *it* was found to agree with, captured
        // inside its own drain. One shared read taken here would be a later screen than either
        // convergence was decided on, and both comparisons would race the shell's next repaint.
        for (label, (events, expected)) in [("first", &first), ("second", &second)] {
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
                String::from_utf8_lossy(expected),
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
    fn version_seven_clients_are_refused_with_an_actionable_message() {
        let runtime = registry();
        let responses = exchange(
            &[&serde_json::to_string(&Request::Hello(HelloRequest { version: 7 })).unwrap()],
            &runtime,
        );
        match &responses[0] {
            Response::Error { code, message } => {
                assert_eq!(*code, ErrorCode::ProtocolMismatch);
                assert!(
                    message.contains(&format!("daemon requires {PROTOCOL_VERSION}")),
                    "the refusal must name the version this daemon speaks: {message}"
                );
            }
            other => panic!("expected protocol mismatch, got {other:?}"),
        }
    }

    #[test]
    fn workspace_socket_lifecycle_is_end_to_end() {
        let runtime = registry();
        let responses = exchange(
            &[
                r#"{"type":"hello","version":8}"#,
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
