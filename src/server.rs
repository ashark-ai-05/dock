use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        io::AsRawFd,
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
        ErrorCode, Event, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, ProcessState, QueueRequest, Request,
        Response,
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
        // The queue's own thread, started here rather than folded into the loop below, because
        // auto-feed must advance whether or not anybody is connected and must advance exactly
        // once per tick however many clients are.
        crate::dispatch::spawn_queue_tick(Arc::clone(&runtime))
            .map_err(|error| format!("could not start the queue thread: {error}"))?;
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
                            // Said out loud rather than dropped. A connection that ends badly
                            // ends for a reason the client cannot see: it learns only that the
                            // socket went away, and on the wrong request at that, because the
                            // daemon answers a message it refuses and *then* hangs up. Without
                            // this, the only account of why was the victim's error message.
                            if let Err(reason) = handle_connection(stream, &runtime) {
                                eprintln!("dockd: client {accepted} disconnected: {reason}");
                            }
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
            Ok(Request::TerminalLaunch(request)) => match runtime.terminal_launch(request) {
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
            Ok(Request::ReportAgentState(request)) => {
                match runtime.report_agent_state(&request.run_id, request.state) {
                    Ok(()) => write_response(stream, &Response::AgentStateRecorded {})?,
                    Err((code, message)) => {
                        write_response(stream, &Response::Error { code, message })?
                    }
                }
            }
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
            Ok(Request::PaneHistory(request)) => {
                // Clamped to what the log can hold: a request for more than that is not an
                // error, it is a caller that does not know the budget, and the honest answer
                // is everything there is.
                let max = (request.max_bytes as usize).min(runtime.pane_history_bytes());
                let served = runtime.with_run_output(&request.run_id, |output| {
                    let (from, bytes, complete) = output.log().before(request.before, max);
                    (output.log().epoch(), from, bytes, complete)
                });
                match served {
                    Some((epoch, from, bytes, complete)) => write_response(
                        stream,
                        &Response::PaneHistory {
                            run_id: request.run_id,
                            epoch,
                            from,
                            bytes: STANDARD.encode(&bytes),
                            complete,
                        },
                    )?,
                    None => write_response(
                        stream,
                        &Response::Error {
                            code: ErrorCode::RunNotFound,
                            message: format!("no live pane {}", request.run_id),
                        },
                    )?,
                }
            }
            Ok(Request::Queue(request)) => {
                let response = match request {
                    QueueRequest::Inspect => Response::Queues {
                        queues: runtime.queue_snapshots(),
                        paused: runtime.queue_paused(),
                    },
                    QueueRequest::Add {
                        workspace_id,
                        pane_id,
                        prompt,
                        label,
                    } => match runtime.queue_add(&workspace_id, &pane_id, label, prompt) {
                        // The whole listing rather than the one entry: the caller almost always
                        // wants to see where its prompt landed in the order, and a second round
                        // trip to find out is the shape of a race.
                        Ok(_) => Response::Queues {
                            queues: runtime.queue_snapshots(),
                            paused: runtime.queue_paused(),
                        },
                        Err((code, message)) => Response::Error { code, message },
                    },
                    QueueRequest::Remove {
                        workspace_id,
                        pane_id,
                        entry_id,
                    } => match runtime.queue_remove(&workspace_id, &pane_id, entry_id) {
                        Ok(()) => Response::Queues {
                            queues: runtime.queue_snapshots(),
                            paused: runtime.queue_paused(),
                        },
                        Err((code, message)) => Response::Error { code, message },
                    },
                    QueueRequest::Clear {
                        workspace_id,
                        pane_id,
                    } => match runtime.queue_clear(&workspace_id, &pane_id) {
                        Ok(_) => Response::Queues {
                            queues: runtime.queue_snapshots(),
                            paused: runtime.queue_paused(),
                        },
                        Err((code, message)) => Response::Error { code, message },
                    },
                    QueueRequest::SetAuto {
                        workspace_id,
                        pane_id,
                        enabled,
                    } => match runtime.queue_set_auto(&workspace_id, &pane_id, enabled) {
                        Ok(()) => Response::Queues {
                            queues: runtime.queue_snapshots(),
                            paused: runtime.queue_paused(),
                        },
                        Err((code, message)) => Response::Error { code, message },
                    },
                    QueueRequest::SetPaused { paused } => match runtime.queue_set_paused(paused) {
                        Ok(()) => Response::Queues {
                            queues: runtime.queue_snapshots(),
                            paused: runtime.queue_paused(),
                        },
                        Err((code, message)) => Response::Error { code, message },
                    },
                };
                write_response(stream, &response)?;
            }
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
    // Queue depth is the one thing on the runs lane that lives only in the daemon, so it is the
    // one thing a subscriber cannot infer from anything else it is already sent. The generation
    // is a single atomic: on the overwhelming majority of passes, where no queue moved, that load
    // is the entire cost of asking.
    let mut queue_generation = 0;
    let mut queue_revisions: HashMap<(String, String), u64> = HashMap::new();
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(());
        }
        if !subscriber_is_present(stream) {
            return Ok(());
        }
        // `pulse` rather than `inspect`: this runs every 16ms for every run, and a full snapshot
        // rebuilds a run's whole identity — around ten strings, two of them formatted from paths —
        // when the loop reads six fields and none of them are those.
        for snapshot in runtime.pulse() {
            // A resize invalidates the row-by-row diff (vt100 zips the two grids, so rows
            // beyond the smaller one would never be transmitted). Re-seed from a full frame
            // instead of diffing across a geometry change.
            if syncs
                .get(&snapshot.run_id)
                .is_some_and(|view| view.size != (snapshot.rows, snapshot.cols))
            {
                syncs.remove(&snapshot.run_id);
            }
            let pane_history_bytes = runtime.pane_history_bytes();
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
                    let (view, history_from, bytes) = *seed;
                    let revision = revisions.get(&snapshot.run_id).copied().unwrap_or(0) + 1;
                    let epoch = view.epoch;
                    write_response(
                        stream,
                        &Response::Stream {
                            event: Event::PaneAttached {
                                run_id: snapshot.run_id.clone(),
                                revision,
                                rows: snapshot.rows,
                                cols: snapshot.cols,
                                history_from,
                                epoch,
                                // Rows the replica must retain to hold everything it can be
                                // sent, derived from the byte budget at a pessimistic 8 bytes
                                // a row so under-sizing (which would silently discard replayed
                                // history off the top) errs towards too many rather than too
                                // few. But it is `PANE_HISTORY_MAX_ROWS` that actually bounds
                                // this: a replica's rows are parsed cells, not raw bytes, and
                                // `vt100` allocates a full row of them — `cols × 32` bytes —
                                // whatever the row holds, so pricing a row at 8 bytes here
                                // understates its real cost by roughly a thousandfold. Without
                                // the cap, the byte budget alone would authorise gigabytes of
                                // cells per pane.
                                scrollback_rows: u32::try_from(
                                    (pane_history_bytes / 8)
                                        .min(crate::terminal::PANE_HISTORY_MAX_ROWS),
                                )
                                .unwrap_or(u32::MAX),
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
        let generation = runtime.queue_generation();
        if generation != queue_generation {
            for (key, revision) in runtime.queue_revisions() {
                if queue_revisions.get(&key) == Some(&revision) {
                    continue;
                }
                write_response(
                    stream,
                    &Response::Stream {
                        event: Event::QueueChanged {
                            workspace_id: key.0.clone(),
                            pane_id: key.1.clone(),
                        },
                    },
                )?;
                // Recorded only once the frame is on the wire, exactly as every other change
                // above is, so a failed write cannot convince this loop the client already knows.
                queue_revisions.insert(key, revision);
            }
            queue_generation = generation;
        }
        thread::sleep(STREAM_POLL_INTERVAL);
    }
}

/// Whether the client at the other end of a subscription is still there.
///
/// `stream_events` never reads from its socket — after `Subscribe` the client sends nothing on it
/// — so the only thing that ever told the push loop its dashboard had gone was a write failing.
/// An idle daemon has nothing to write, which is exactly the case that matters: a dashboard killed
/// while its panes were quiet left the loop polling every run sixty-two times a second, and
/// spawning a `ps` on its behalf, for the rest of the daemon's life. That is what a forgotten
/// daemon burning CPU while apparently idle turned out to be.
///
/// A zero-length `send` is the probe, because EOF is not one. Reading EOF cannot tell a client
/// that has gone from one that has merely shut down the write half it was never going to use
/// again, and on macOS `poll` reports `POLLHUP` for both — measured, not assumed. A zero-length
/// send puts nothing on the wire, succeeds for a peer that is only half-closed, and fails with
/// `EPIPE` once the peer is really gone. Anything else it might fail with is treated as present,
/// so a probe that cannot answer never ends a live subscription.
fn subscriber_is_present(stream: &UnixStream) -> bool {
    let nothing: [u8; 0] = [];
    // SAFETY: `send` reads `len` bytes from the buffer it is given and writes nothing through it.
    // `len` is zero, so the pointer is never dereferenced, and it is a live non-null pointer to a
    // zero-length array regardless. The fd is owned by `stream` and outlives the call.
    let sent = unsafe { nix::libc::send(stream.as_raw_fd(), nothing.as_ptr().cast(), 0, 0) };
    sent >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(nix::libc::EPIPE)
}

/// What one poll owes one subscriber for one run.
enum StreamFrame {
    /// The child's own bytes since this subscriber's last frame, with any repaint needed to
    /// erase drift appended. Empty when the pane has been silent, which is the common case.
    Delta(Vec<u8>),
    /// A full snapshot and the view that replaces this subscriber's, for a run it has never
    /// seen, one whose geometry changed, or one whose output it has fallen behind. The `u64`
    /// is the sequence the replayed bytes begin at: the sequence a client would need to name
    /// to page further back than this seed reaches.
    /// Boxed only to keep the common `Delta` frame small: a view carries two parsers.
    Seed(Box<(SubscriberView, u64, Vec<u8>)>),
}

/// How much retained output rides along with an attach frame.
///
/// Enough that scrolling up is instant for the distance anyone scrolls without thinking, and
/// small enough that attaching to a canvas of panes is not a stall: this is paid per pane, on
/// every client start and every re-seed. Everything older is paged in on demand.
const SEED_HISTORY_BYTES: usize = 256 * 1024;

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
    /// A fresh subscriber's starting point: a replay of the pane's recent history, followed by
    /// whatever correction that replay did not achieve on its own.
    ///
    /// Replaying raw history rather than sending the visible grid is what gives the client
    /// something to scroll back through: a repaint is cursor-addressed and never scrolls a row
    /// into scrollback, so a client seeded with one begins with no history at all.
    ///
    /// **The alternate screen is handled by comparison, not by assumption.** The replayed bytes
    /// may themselves contain `1049h`/`1049l`, so after replay this subscriber's parser can be
    /// in either buffer, and the older rule here — always land in a fresh primary buffer, so
    /// only `1049h` is ever needed — no longer holds. Instead the seed's own `ScreenSync` is
    /// asked which buffer the replay reached, and the corrective sequence is appended only when
    /// it disagrees with the live screen. Both directions matter: a replica left in the
    /// alternate screen paints a full-screen program over the user's history, and a replica left
    /// on primary renders a full-screen program into scrollback.
    fn seeded(output: &PaneOutput, rows: u16, cols: u16) -> (Self, u64, Vec<u8>) {
        let (from, mut bytes) = output.log().tail(SEED_HISTORY_BYTES);
        let mut view = Self {
            sync: ScreenSync::new(rows, cols),
            size: (rows, cols),
            offset: output.log().end(),
            epoch: output.log().epoch(),
        };
        view.sync.apply(&bytes);
        if view.sync.alternate_screen() != output.screen().alternate_screen() {
            let correction: &[u8] = if output.screen().alternate_screen() {
                b"\x1b[?1049h"
            } else {
                b"\x1b[?1049l"
            };
            bytes.extend_from_slice(correction);
            view.sync.apply(correction);
        }
        // The replayed tail is not guaranteed to reproduce the live screen: it can be
        // truncated (the oldest rows aged out of the retained log), and it can be silent
        // where the live screen changed without a byte to replay at all — a resize reflows
        // the screen without appending to the log. Left uncorrected, either leaves the
        // replica showing something the daemon does not, with nothing further ever arriving
        // to fix it: a silent pane has no future delta to carry the repair. This is the same
        // primitive `next_delta` uses to erase drift; it is cursor-addressed, so it adds
        // nothing to the replica's history.
        let repaint = view.sync.delta_from(output.screen());
        if !repaint.is_empty() {
            view.sync.apply(&repaint);
            bytes.extend_from_slice(&repaint);
        }
        (view, from, bytes)
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

/// Writes one response as a single message.
///
/// Serialised in full first, deliberately. `serde_json::to_writer` on the socket emits the value
/// in small pieces — `{`, `"`, `type`, and so on — each its own write, so a daemon that dies
/// partway leaves a fragment like `{"` on the wire. The client then reports a JSON parse error
/// about column 2, which says nothing about what actually happened: the daemon went away. One
/// write makes the message all-or-nothing, and a departed daemon reads as the clean end of a
/// connection, which callers already handle and can explain.
fn write_response(stream: &mut UnixStream, response: &Response) -> Result<(), String> {
    let mut message = serde_json::to_vec(response).map_err(|error| error.to_string())?;
    message.push(b'\n');
    stream.write_all(&message).map_err(|error| {
        // A client that stops reading is worth naming, because the shape of the failure hides
        // it: replies it never collects fill the socket, this write blocks, the write timeout
        // ends it, and the connection closes. The client then fails on whatever it sent next,
        // which is never the thing at fault. "Broken pipe" told nobody any of that.
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            return format!(
                "client stopped reading its replies, so a {}s write timed out and the \
                 connection was closed; it will see this as a broken pipe on its next request",
                CLIENT_WRITE_TIMEOUT.as_secs()
            );
        }
        error.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        HelloRequest, PaneHistoryRequest, PaneInputRequest, PaneResizeRequest, SubscribeRequest,
    };
    use crate::terminal::PaneScreen;
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
        registry_with_scrollback(64)
    }

    /// Sixty-four rows is enough for a test that only wants a pane to exist. A measurement wants
    /// the retained history a real daemon carries, because the cost of handing a subscriber its
    /// bytes is bounded by it.
    fn registry_with_scrollback(scrollback_rows: usize) -> TestRegistry {
        let state = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dock-registry-test-{}-{}",
                std::process::id(),
                SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        let registry = RuntimeRegistry::new(&state, scrollback_rows).expect("test registry");
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
        let deadline = crate::testing::deadline(2);
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
    fn test_stream_window() -> Duration {
        crate::testing::budget_millis(400)
    }

    fn exchange(lines: &[&str], runtime: &RuntimeRegistry) -> Vec<Response> {
        exchange_within(lines, runtime, test_stream_window())
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
                    &format!(r#"{{"type":"hello","version":{PROTOCOL_VERSION},"future":true}}"#),
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
        //
        // Both bounds below are deliberately loose, because the deadline this test exists to rule
        // out is the ten minutes given to `idle`. Discriminating at five seconds made two claims
        // the test never set out to make — that the daemon answers a hello within five seconds,
        // and that a thread is scheduled within five seconds of the socket closing — and on a
        // machine running the whole suite at once neither is a claim worth staking a red build on.
        // A bound two orders of magnitude under the deadline separates "noticed the EOF" from
        // "waited out the deadline" just as decisively, and separates nothing else.
        let idle = Duration::from_secs(600);
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
                    idle,
                    in_flight: idle,
                },
                None,
            )
        });
        client
            .set_read_timeout(Some(crate::testing::budget(30)))
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
        // Says what went wrong rather than `Any { .. }`. This has failed during whole-suite runs
        // and left behind nothing but the fact that the handler had not returned `Ok`, which is
        // not enough to tell a missed EOF from an exhausted machine. Whatever ends it next — the
        // error the daemon reported, or the message it panicked with — belongs in the failure.
        let outcome = handler.join().unwrap_or_else(|panic| {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a payload that is not a string".into());
            panic!("the connection handler panicked: {detail}");
        });
        let reclaimed = start.elapsed();
        assert_eq!(
            outcome,
            Ok(()),
            "a departed client must end its handler cleanly on EOF"
        );
        assert!(
            reclaimed < crate::testing::budget(30),
            "a departed client waited {reclaimed:?} to be noticed, out of an idle bound of \
             {idle:?}, so a deadline reclaimed it rather than its EOF"
        );
        assert_eq!(
            admission.active.load(Ordering::Acquire),
            0,
            "the handler's permit must be released when the connection ends"
        );
    }

    /// The stale-daemon defect. `stream_events` never reads from its socket — after `Subscribe` a
    /// client sends nothing on it — so the only thing that ever told the push loop its dashboard
    /// had gone was a write failing, and an idle daemon has nothing to write. A dashboard killed
    /// while its panes were quiet left this loop polling every run sixty-two times a second, and
    /// spawning a `ps` on its behalf twice a second, for the rest of the daemon's life: measured
    /// at 10.7% of a core, sustained, by a daemon nobody was talking to.
    ///
    /// Deliberately not asserted through EOF, which is what the first attempt used. A client that
    /// has gone and one that has merely shut down the write half it was never going to use again
    /// read identically — this suite's own `exchange` does the second — so EOF would have ended
    /// live subscriptions.
    #[test]
    fn a_client_that_stops_reading_is_named_rather_than_left_to_guess() {
        // The failure this exists for is silent on the daemon's side and misattributed on the
        // client's: replies nobody collects fill the socket, the daemon's own write blocks, the
        // write timeout closes the connection, and the client then fails on whatever request it
        // sent next. A person reading the daemon's output saw nothing at all.
        let (client, mut server) = UnixStream::pair().expect("socket pair");
        // A write deadline short enough to reach, and a peer that never reads.
        server
            .set_write_timeout(Some(Duration::from_millis(50)))
            .expect("write timeout");
        let big = Response::Error {
            code: ErrorCode::RunNotFound,
            message: "x".repeat(4096),
        };
        let mut reason = None;
        for _ in 0..64 {
            if let Err(error) = write_response(&mut server, &big) {
                reason = Some(error);
                break;
            }
        }
        let reason = reason.expect("a peer that never reads must eventually stall the write");
        assert!(
            reason.contains("stopped reading"),
            "the daemon must name the cause, got {reason:?}"
        );
        assert!(
            reason.contains("broken pipe"),
            "and say what the client will see, got {reason:?}"
        );
        drop(client.take_error());
    }

    #[test]
    fn a_subscriber_whose_client_has_gone_stops_being_polled() {
        let runtime = registry();
        create_workspace(&runtime);
        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        let shared = runtime.shared();
        let handler = thread::spawn(move || {
            handle_connection_with_timeout(
                &mut server,
                &shared,
                ReadTimeouts::PRODUCTION,
                // Long enough that reaching it would be a failure rather than the answer: the
                // loop has to stop because its client left, not because it ran out of time.
                Some(Instant::now() + crate::testing::budget(120)),
            )
        });
        client
            .set_read_timeout(Some(crate::testing::budget(30)))
            .expect("read timeout");
        let mut reader = BufReader::new(client.try_clone().expect("clone client"));
        send_line(&mut client, &hello());
        assert!(matches!(
            next_response(&mut reader),
            Response::Hello {
                version: PROTOCOL_VERSION
            }
        ));
        send_line(&mut client, &subscribe_line());
        // Waits for the first pushed frame, so the loop is certainly streaming before its client
        // walks away. Without this the test could pass by ending the connection before it began.
        assert!(matches!(
            next_response(&mut reader),
            Response::Stream { .. }
        ));
        drop(reader);
        drop(client);

        let started = Instant::now();
        assert_eq!(
            handler.join().expect("the push loop must not panic"),
            Ok(()),
            "a subscriber whose client has gone must end its loop cleanly"
        );
        assert!(
            started.elapsed() < crate::testing::budget(30),
            "the push loop took {:?} to notice its client had gone",
            started.elapsed()
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
        let hello = format!(r#"{{"type":"hello","version":{PROTOCOL_VERSION}}}"#);
        let dispatched = exchange(&[&hello, &dispatch], &runtime);
        let pid = match &dispatched[1] {
            Response::Dispatched { snapshot } => snapshot.pid,
            response => panic!("unexpected response: {response:?}"),
        };
        assert!(pid.is_some());
        let inspect = r#"{"type":"inspect","run_id":"dock_socket_reconnect"}"#;
        let first = exchange(&[&hello, inspect], &runtime);
        let second = exchange(&[&hello, inspect], &runtime);
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
        let hello = format!(r#"{{"type":"hello","version":{PROTOCOL_VERSION}}}"#);
        let requests = [hello.as_str(), r#"{"type":"inspect"}"#];
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
        let ticks = test_stream_window().as_millis() / STREAM_POLL_INTERVAL.as_millis();
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
        let deadline = Instant::now() + convergence_backstop();
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
    fn convergence_backstop() -> Duration {
        crate::testing::budget(15)
    }

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
        let deadline = Instant::now() + convergence_backstop();
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
                    Instant::now() + crate::testing::budget(1) < deadline,
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
            (runtime.pane_history_bytes() / 8).min(crate::terminal::PANE_HISTORY_MAX_ROWS),
            "the attach frame must carry a capacity derived from the daemon's real history \
             retention, not a client-side guess"
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

    /// A pane that has written more than a seed replays retains a cursor to page back from
    /// (Task 3), and `PaneHistory` is what lets a client actually use it: asked for the bytes
    /// behind that cursor, the daemon must answer from the same byte stream the attach frame
    /// named (`epoch`), never run its answer past the caller's own cursor, and say so when the
    /// answer reaches everything still retained.
    #[test]
    fn a_pane_history_request_answers_from_the_cursor_an_attach_frame_reported() {
        let runtime = registry();
        create_workspace(&runtime);
        let run_id = runtime
            .inspect(None)
            .expect("inspect")
            .into_iter()
            .find(|run| run.pane_id == "p1")
            .expect("bound run")
            .run_id;

        // More than a seed ever replays, so the attach frame below truncates its cursor away
        // from the very start of the log and leaves genuine history behind it to page back
        // into. Comfortably short of the daemon's own retention budget, so every one of these
        // bytes is still there to be served back in full.
        runtime
            .pane_input("w1", "p1", b"yes | head -c 300000; echo DONEMARK\n")
            .expect("type into the pane");
        let deadline = Instant::now() + convergence_backstop();
        while runtime
            .with_run_output(&run_id, |output| output.log().end())
            .expect("live output")
            < (SEED_HISTORY_BYTES as u64 + 4096)
        {
            assert!(
                Instant::now() < deadline,
                "the shell never produced enough output to truncate the seed"
            );
            thread::sleep(Duration::from_millis(20));
        }

        let responses = exchange(&[&hello(), &subscribe_line()], &runtime);
        let (attached_history_from, attached_epoch) = collect_events(&responses)
            .into_iter()
            .find_map(|event| match event {
                Event::PaneAttached {
                    history_from,
                    epoch,
                    ..
                } => Some((history_from, epoch)),
                _ => None,
            })
            .expect("an attach frame");

        let request = Request::PaneHistory(PaneHistoryRequest {
            run_id: run_id.clone(),
            before: attached_history_from,
            max_bytes: 2 << 20,
        });
        let responses = exchange(
            &[&hello(), &serde_json::to_string(&request).unwrap()],
            &runtime,
        );
        match &responses[1] {
            Response::PaneHistory {
                epoch,
                from,
                complete,
                bytes,
                ..
            } => {
                assert_eq!(*epoch, attached_epoch, "the same byte stream");
                assert!(*from <= attached_history_from);
                assert!(
                    *complete,
                    "a fixture well inside the daemon's retention budget keeps everything it \
                     wrote"
                );
                assert!(!STANDARD.decode(bytes).expect("base64").is_empty());
            }
            other => panic!("expected history, got {other:?}"),
        }
    }

    #[test]
    fn history_for_a_run_the_daemon_does_not_have_is_refused_rather_than_answered_empty() {
        let runtime = registry();
        let request = Request::PaneHistory(PaneHistoryRequest {
            run_id: "no-such-run".into(),
            before: 0,
            max_bytes: 1 << 20,
        });
        let responses = exchange(
            &[&hello(), &serde_json::to_string(&request).unwrap()],
            &runtime,
        );
        match &responses[1] {
            Response::Error { code, .. } => assert_eq!(
                *code,
                ErrorCode::RunNotFound,
                "an empty answer and a missing pane must not look the same to a client"
            ),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn a_seed_carries_the_panes_history_and_not_just_its_visible_screen() {
        let mut output = PaneOutput::new(2, 20, 100, 4096);
        for line in 0..20 {
            output.feed(format!("line {line}\r\n").as_bytes());
        }
        let (_, _, bytes) = SubscriberView::seeded(&output, 2, 20);
        let seeded = String::from_utf8_lossy(&bytes);
        assert!(
            seeded.contains("line 0"),
            "the seed must replay history, not just the two visible rows: {seeded:?}"
        );
    }

    #[test]
    fn a_seed_whose_history_ends_in_the_alternate_screen_returns_a_primary_pane_to_primary() {
        let mut output = PaneOutput::new(4, 20, 100, 4096);
        output.feed(b"history\r\n");
        output.feed(b"\x1b[?1049h"); // a full-screen program starts
        output.feed(b"inside the program");
        output.feed(b"\x1b[?1049l"); // and exits, leaving the pane on primary
        assert!(!output.screen().alternate_screen());
        let (_, _, bytes) = SubscriberView::seeded(&output, 4, 20);
        let mut replica = PaneScreen::new(4, 20, 100);
        replica.feed(&bytes);
        assert!(
            !replica.alternate_screen(),
            "a replica left in the alternate screen would paint over the user's history"
        );
    }

    #[test]
    fn a_seed_for_a_pane_inside_the_alternate_screen_puts_the_replica_there() {
        let mut output = PaneOutput::new(4, 20, 100, 4096);
        output.feed(b"history\r\n");
        output.feed(b"\x1b[?1049h");
        output.feed(b"inside the program");
        assert!(output.screen().alternate_screen());
        let (_, _, bytes) = SubscriberView::seeded(&output, 4, 20);
        let mut replica = PaneScreen::new(4, 20, 100);
        replica.feed(&bytes);
        assert!(replica.alternate_screen());
    }

    /// A resize reflows the daemon's screen without appending a single byte to the log, so
    /// replaying the retained tail can never reproduce it, no truncation required. A seed that
    /// only replayed history would leave a subscriber on a pre-resize screen it has no future
    /// delta to correct, since a silent pane sends nothing further to carry the repair.
    #[test]
    fn a_seed_repaints_whatever_the_replayed_tail_alone_could_not_reproduce() {
        let mut output = PaneOutput::new(4, 20, 100, 4096);
        output.feed(b"one two three four five\r\nsix seven eight\r\n");
        // Reflows the live screen in place; the log above is untouched by it.
        output.screen_mut().resize(4, 10);
        let (_, _, bytes) = SubscriberView::seeded(&output, 4, 10);
        let mut replica = PaneScreen::new(4, 10, 100);
        replica.feed(&bytes);
        assert_eq!(
            replica.state_bytes(),
            output.screen().state_bytes(),
            "a seed must repaint whatever its replayed tail alone could not reproduce"
        );
    }

    /// What attaching a subscriber to a pane with a full history costs.
    ///
    /// This is paid per pane on every client start and every re-seed, so it is the number that
    /// decides whether the seed prefix is the right size. Fastest of several rounds, for the
    /// reason `measure_frame` gives: noise only ever makes a round slower.
    #[test]
    #[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture"]
    fn measure_what_seeding_a_pane_with_its_history_costs() {
        let mut output = PaneOutput::new(40, 160, 100_000, crate::terminal::PANE_HISTORY_BYTES);
        for line in 0..200_000 {
            output.feed(format!("line {line} of a long build log\r\n").as_bytes());
        }
        let mut fastest = f64::MAX;
        let mut size = 0;
        for _ in 0..7 {
            let start = std::time::Instant::now();
            let (_, _, bytes) = SubscriberView::seeded(&output, 40, 160);
            fastest = fastest.min(start.elapsed().as_secs_f64() * 1000.0);
            size = bytes.len();
        }
        println!("\nseed of a full pane: {size} bytes in {fastest:.2}ms");
    }

    /// A subscriber slow enough to fall past the retained window must be re-seeded. Serving it
    /// whatever bytes survive would skip the rest, and a mirror missing a run of bytes looks
    /// exactly like a rendering bug with nothing to attribute it to.
    #[test]
    fn a_subscriber_that_falls_behind_the_retained_output_is_re_seeded_rather_than_skipped() {
        let mut output = PaneOutput::new(5, 20, 100, 16);
        output.feed(b"first\r\n");
        let (mut view, _history_from, seed) = SubscriberView::seeded(&output, 5, 20);
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
        let (mut recovered, _history_from, reseed) = SubscriberView::seeded(&output, 5, 20);
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
        let deadline = Instant::now() + convergence_backstop();
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
                    Instant::now() + crate::testing::budget(1) < deadline,
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

    /// The whole queue surface across the socket in one exchange, because this is the shape a
    /// client is being wired to: every operation answers with the *complete* listing rather than
    /// with an acknowledgement, so a board never has to make a second round trip to find out where
    /// its prompt landed in the order.
    #[test]
    fn every_queue_operation_answers_with_the_whole_listing() {
        let runtime = registry();
        create_workspace(&runtime);
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::Queue(QueueRequest::Add {
                    workspace_id: "w1".into(),
                    pane_id: "p1".into(),
                    prompt: "keep going".into(),
                    label: "card 7".into(),
                }))
                .unwrap(),
                &serde_json::to_string(&Request::Queue(QueueRequest::SetPaused { paused: true }))
                    .unwrap(),
                &serde_json::to_string(&Request::Queue(QueueRequest::Inspect)).unwrap(),
                &serde_json::to_string(&Request::Queue(QueueRequest::Clear {
                    workspace_id: "w1".into(),
                    pane_id: "p1".into(),
                }))
                .unwrap(),
            ],
            &runtime,
        );
        let Response::Queues { queues, paused } = &responses[1] else {
            panic!("an add answers with the listing: {:?}", responses[1]);
        };
        assert_eq!(queues.len(), 1);
        assert_eq!(queues[0].entries[0].label, "card 7");
        assert_eq!(queues[0].entries[0].preview, "keep going");
        assert_eq!(queues[0].entries[0].bytes, "keep going".len());
        assert!(
            !queues[0].auto_feed,
            "nothing that crosses this socket arms a pane but `set_auto`"
        );
        assert!(!paused);
        let Response::Queues { paused, .. } = &responses[3] else {
            panic!("inspect answers with the listing: {:?}", responses[3]);
        };
        assert!(
            paused,
            "the pause is daemon-wide and reported with the queues"
        );
        let Response::Queues { queues, .. } = &responses[4] else {
            panic!("a clear answers with the listing: {:?}", responses[4]);
        };
        assert!(queues[0].entries.is_empty());
    }

    /// The refusals reach the client as refusals, with their own code. `GateBlocked` already
    /// carries five distinct meanings and a sixth would make it useless for diagnosis.
    #[test]
    fn a_refused_queue_request_crosses_the_socket_as_a_queue_refusal() {
        let runtime = registry();
        create_workspace(&runtime);
        let responses = exchange(
            &[
                &hello(),
                &serde_json::to_string(&Request::Queue(QueueRequest::SetAuto {
                    workspace_id: "w1".into(),
                    pane_id: "p1".into(),
                    enabled: true,
                }))
                .unwrap(),
            ],
            &runtime,
        );
        let Response::Error { code, message } = &responses[1] else {
            panic!(
                "arming an unhooked agent must be refused: {:?}",
                responses[1]
            );
        };
        assert_eq!(*code, ErrorCode::QueueRefused);
        assert!(message.contains("dock hooks --install"));
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
                &format!(r#"{{"type":"hello","version":{PROTOCOL_VERSION}}}"#),
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

    // ---------------------------------------------------------------------------------------
    // Measurement harness.
    //
    // Not a test: nothing below asserts anything. It exists because every performance claim
    // made about this daemon before it was written was made from reading, and two of them were
    // wrong — the process table was believed to cost the daemon nothing once it was cached, and
    // the classification memo was believed to be a memo. Run it with
    //
    //     cargo test --release --lib -- --ignored --nocapture measure_the_daemon_hot_path
    //
    // and compare the numbers a change claims to move against the numbers it actually moved.
    // ---------------------------------------------------------------------------------------

    /// How many panes the measurement drives, overridable with `DOCK_BENCH_PANES`.
    ///
    /// Sixteen is a dashboard somebody actually has open: four workspaces of four panes. The
    /// brief's range is twelve to thirty and the interesting costs are all linear in this, so a
    /// number in the middle reads the slope as well as either end would.
    fn bench_panes() -> usize {
        std::env::var("DOCK_BENCH_PANES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(16)
    }

    /// CPU seconds burned by this process and, separately, by the children it has reaped.
    ///
    /// Both halves matter, and the second is the one that hid the problem. The daemon learns
    /// which agent runs under a pane by spawning `ps`, and a subprocess's CPU is charged to the
    /// subprocess — so a daemon paying for a 35ms `ps` twice a second shows almost nothing in
    /// `top` against its own name. Reading only `RUSAGE_SELF` is how an idle daemon came to look
    /// free while a core was being spent on its behalf.
    fn cpu_seconds() -> (f64, f64) {
        fn read(who: i32) -> f64 {
            // SAFETY: `getrusage` writes a fully-initialised `rusage` into the pointer it is
            // given and reads nothing else. The zeroed value is a valid `rusage`.
            let mut usage: nix::libc::rusage = unsafe { std::mem::zeroed() };
            let taken = unsafe { nix::libc::getrusage(who, &raw mut usage) };
            if taken != 0 {
                return 0.0;
            }
            let seconds = |value: nix::libc::timeval| {
                value.tv_sec as f64 + value.tv_usec as f64 / 1_000_000.0
            };
            seconds(usage.ru_utime) + seconds(usage.ru_stime)
        }
        (
            read(nix::libc::RUSAGE_SELF),
            read(nix::libc::RUSAGE_CHILDREN),
        )
    }

    /// One line of timing statistics, in the units a 16ms poll is judged against.
    fn report_durations(label: &str, panes: usize, mut samples: Vec<Duration>) {
        samples.sort_unstable();
        let micros = |value: Duration| value.as_secs_f64() * 1_000.0;
        let at = |fraction: f64| {
            micros(samples[((samples.len() as f64 - 1.0) * fraction).round() as usize])
        };
        let total: f64 = samples.iter().map(|sample| micros(*sample)).sum();
        let mean = total / samples.len() as f64;
        println!(
            "{label:<44} n={:<5} mean={mean:7.3}ms  p50={:7.3}ms  p99={:7.3}ms  max={:7.3}ms  \
             per-pane-mean={:7.3}ms",
            samples.len(),
            at(0.5),
            at(0.99),
            at(1.0),
            mean / panes as f64,
        );
    }

    /// A workspace of `panes` real pane shells, sized as a dashboard would size them.
    fn bench_workspace(runtime: &RuntimeRegistry, panes: usize) -> Vec<String> {
        runtime
            .workspace(crate::protocol::WorkspaceRequest::Create {
                workspace_id: "bench".into(),
                name: "Bench".into(),
                pane_id: "p0".into(),
            })
            .expect("create the bench workspace");
        let mut pane_ids = vec!["p0".to_owned()];
        for index in 1..panes {
            let pane_id = format!("p{index}");
            runtime
                .workspace(crate::protocol::WorkspaceRequest::Split {
                    workspace_id: "bench".into(),
                    pane_id: pane_ids[index - 1].clone(),
                    new_pane_id: pane_id.clone(),
                    axis: crate::layout::SplitAxis::Vertical,
                    kind: crate::layout::PaneKind::Terminal,
                })
                .expect("split a bench pane");
            pane_ids.push(pane_id);
        }
        // A pane the dashboard has never measured is 24x80. A real one is not, and the whole
        // screen is what classification reads, so measuring at the default would understate
        // every cost that scales with cell count.
        for pane_id in &pane_ids {
            runtime
                .pane_resize("bench", pane_id, 40, 160)
                .expect("size a bench pane like a dashboard would");
        }
        pane_ids
    }

    #[test]
    #[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture"]
    fn measure_the_daemon_hot_path_under_a_dashboard_sized_load() {
        let panes = bench_panes();
        let runtime = registry_with_scrollback(2_000);
        let pane_ids = bench_workspace(&runtime, panes);
        // The queue thread, started exactly as `serve` starts it, so what this measures is the
        // daemon a user actually runs. No queues are created: that is the state every daemon is in
        // until somebody queues something, and it is the state this harness has to keep measuring
        // if its numbers are to stay comparable across releases. What a *populated* queue costs is
        // its own measurement — `measure_what_the_queue_tick_costs_over_every_run` — because it is
        // a different question with a different answer.
        let _queue_thread =
            crate::dispatch::spawn_queue_tick(runtime.shared()).expect("start the queue thread");
        // Long enough for every shell to have execed, painted its prompt and gone quiet, so the
        // idle phase below measures an idle daemon rather than sixteen starting ones.
        thread::sleep(Duration::from_millis(2_500));
        println!("\n--- {panes} panes, {} rows of scrollback ---", 2_000);

        for phase in [BenchPhase::Idle, BenchPhase::Streaming] {
            let feeding = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let feeder = matches!(phase, BenchPhase::Streaming).then(|| {
                let shared = runtime.shared();
                let pane_ids = pane_ids.clone();
                let feeding = Arc::clone(&feeding);
                thread::spawn(move || {
                    // Typed into the pane rather than run by it: the line discipline echoes
                    // every byte straight back out of the PTY, which is output arriving in the
                    // emulator continuously and under the harness's control. Running a printing
                    // loop in each shell instead would measure the machine's ability to fork
                    // `sleep` sixteen times a frame, not the daemon.
                    while feeding.load(std::sync::atomic::Ordering::Relaxed) {
                        for pane_id in &pane_ids {
                            let _ = shared.pane_input(
                                "bench",
                                pane_id,
                                b"streaming a line of agent output into the pane\r",
                            );
                        }
                        thread::sleep(Duration::from_millis(32));
                    }
                })
            });
            thread::sleep(Duration::from_millis(400));
            let label = phase.label();

            let mut pulses = Vec::new();
            let mut inspects = Vec::new();
            let started = Instant::now();
            let (self_before, children_before) = cpu_seconds();
            while started.elapsed() < Duration::from_secs(4) {
                let tick = Instant::now();
                let _ = runtime.pulse();
                pulses.push(tick.elapsed());
                thread::sleep(STREAM_POLL_INTERVAL);
            }
            let (self_after, children_after) = cpu_seconds();
            let window = started.elapsed().as_secs_f64();
            for _ in 0..40 {
                let tick = Instant::now();
                let _ = runtime.inspect(None);
                inspects.push(tick.elapsed());
                thread::sleep(STREAM_POLL_INTERVAL);
            }

            report_durations(&format!("pulse [{label}]"), panes, pulses);
            report_durations(&format!("inspect [{label}]"), panes, inspects);
            println!(
                "cpu [{label:<9}]                             daemon={:5.2}%  spawned `ps` and \
                 pane shells={:5.2}%  total={:5.2}%",
                (self_after - self_before) / window * 100.0,
                (children_after - children_before) / window * 100.0,
                (self_after - self_before + children_after - children_before) / window * 100.0,
            );

            feeding.store(false, std::sync::atomic::Ordering::Relaxed);
            if let Some(feeder) = feeder {
                feeder.join().expect("stop the feeder");
            }
            thread::sleep(Duration::from_millis(1_500));
        }
    }

    #[derive(Clone, Copy)]
    enum BenchPhase {
        Idle,
        Streaming,
    }

    impl BenchPhase {
        fn label(self) -> &'static str {
            match self {
                Self::Idle => "idle",
                Self::Streaming => "streaming",
            }
        }
    }

    /// What the queue's 250ms tick actually costs, in the two states that matter.
    ///
    /// The hot-path measurement above runs the tick alongside everything else, which answers "did
    /// the daemon get slower". This answers the narrower question the design turns on: a tick is
    /// recurring work over every run, and the claim is that it costs nothing at all until somebody
    /// queues something and very little afterwards. Both halves are printed as a duty cycle —
    /// what fraction of each 250ms period the tick is actually running — because that, not the
    /// millisecond figure, is what the daemon pays.
    #[test]
    #[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture"]
    fn measure_what_the_queue_tick_costs_over_every_run() {
        let panes = bench_panes();
        let runtime = registry_with_scrollback(2_000);
        let pane_ids = bench_workspace(&runtime, panes);
        thread::sleep(Duration::from_millis(2_500));
        println!("\n--- {panes} panes, one queue tick every 250ms ---");

        let sample = |label: &str| {
            let mut ticks = Vec::new();
            let (self_before, children_before) = cpu_seconds();
            let started = Instant::now();
            for _ in 0..40 {
                let tick = Instant::now();
                runtime.queue_tick();
                ticks.push(tick.elapsed());
                thread::sleep(crate::dispatch::QUEUE_TICK_INTERVAL);
            }
            let window = started.elapsed().as_secs_f64();
            let (self_after, children_after) = cpu_seconds();
            let busy: f64 = ticks.iter().map(|tick| tick.as_secs_f64()).sum();
            report_durations(&format!("queue_tick [{label}]"), panes, ticks);
            println!(
                "queue_tick [{label:<12}]                  duty={:6.3}%  daemon={:5.2}%  spawned \
                 `ps` and pane shells={:5.2}%",
                busy / window * 100.0,
                (self_after - self_before) / window * 100.0,
                (children_after - children_before) / window * 100.0,
            );
        };

        // Every daemon, until somebody queues something: one lock and one length comparison.
        sample("no queues");
        for pane_id in &pane_ids {
            runtime
                .queue_add("bench", pane_id, "bench".into(), "keep going".into())
                .expect("queue a prompt for the bench pane");
        }
        // The worst case the caps allow at this pane count: every pane holding a queue, so every
        // tick pulses, maps and polls all of them. Left unarmed, because an unarmed queue is
        // polled in full and typing into sixteen shells would measure the shells.
        sample("every pane");
    }

    #[test]
    #[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture"]
    fn measure_what_a_subscriber_whose_client_has_gone_still_costs() {
        // The stale-daemon question. `stream_events` never reads from its socket, so a
        // subscriber whose dashboard died is noticed only when a write fails — and an idle
        // daemon has nothing to write. This measures what that loop costs while nobody is
        // listening, which is what a forgotten daemon costs for the rest of its life.
        let panes = bench_panes();
        let runtime = registry_with_scrollback(2_000);
        bench_workspace(&runtime, panes);
        thread::sleep(Duration::from_millis(2_500));
        println!("\n--- {panes} idle panes, subscriber's client gone ---");

        let (mut client, mut server) = UnixStream::pair().expect("socket pair");
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("read timeout");
        send_line(&mut client, &hello());
        send_line(&mut client, &subscribe_line());
        let shared = runtime.shared();
        let window = Duration::from_secs(6);
        let handler = thread::spawn(move || {
            handle_connection_with_timeout(
                &mut server,
                &shared,
                ReadTimeouts::PRODUCTION,
                // Ample headroom: the question this asks is whether the loop stops when its
                // client leaves, and a deadline that could plausibly have stopped it first
                // would answer a different one.
                Some(Instant::now() + window + Duration::from_secs(30)),
            )
        });
        // Let the seed frames land, then walk away exactly as a killed dashboard does.
        thread::sleep(Duration::from_millis(1_000));
        drop(client);

        let started = Instant::now();
        let (self_before, children_before) = cpu_seconds();
        thread::sleep(window);
        let (self_after, children_after) = cpu_seconds();
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "cpu with a departed subscriber                daemon={:5.2}%  spawned `ps`={:5.2}%  \
             total={:5.2}%",
            (self_after - self_before) / elapsed * 100.0,
            (children_after - children_before) / elapsed * 100.0,
            (self_after - self_before + children_after - children_before) / elapsed * 100.0,
        );
        let still_running = !handler.is_finished();
        println!("the push loop is still running after the client left: {still_running}");
        let _ = handler.join();
    }
}
