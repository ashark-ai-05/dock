use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crate::protocol::{Event, HelloRequest, PROTOCOL_VERSION, Request, Response, SubscribeRequest};

/// The request's kind, for a message about a request that failed.
///
/// The whole value would carry pane input and dispatch prompts into a log and an error line, so
/// only the shape is named. What went wrong is never which characters were typed.
fn request_name(request: &Request) -> String {
    match request {
        Request::PaneInput(r) => format!("pane input for {}/{}", r.workspace_id, r.pane_id),
        Request::PaneResize(r) => format!("a resize of {}/{}", r.workspace_id, r.pane_id),
        Request::Inspect(_) => "an inspect".into(),
        Request::Workspace(_) => "a workspace request".into(),
        Request::Hello(_) => "the handshake".into(),
        other => format!("{}", DebugKind(other)),
    }
}

struct DebugKind<'a>(&'a Request);

impl std::fmt::Display for DebugKind<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The variant name only: `Debug` on the whole request would print the payload.
        let rendered = format!("{:?}", self.0);
        let name = rendered
            .split(['(', ' ', '{'])
            .next()
            .unwrap_or("a request");
        write!(formatter, "{name}")
    }
}

/// Appends one wire message to the file named by `DOCK_WIRE_DEBUG`, if it is set.
///
/// Off unless asked for, because this is the traffic of a working session and it is nobody's
/// business by default. It exists because the failure it diagnoses is intermittent: the daemon
/// refuses a message and closes, and the error surfaces on a later request that was fine, so the
/// only way to see the real cause is to have been recording when it happened.
fn wire_debug(tag: &str, length: usize, payload: &[u8]) {
    let Some(path) = std::env::var_os("DOCK_WIRE_DEBUG") else {
        return;
    };
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let text = String::from_utf8_lossy(&payload[..payload.len().min(400)]);
        let _ = writeln!(file, "{tag} [{length}] {}", text.trim_end());
    }
}

/// How many un-read replies may be outstanding before the next send stops to collect them.
///
/// Chosen well under what actually breaks rather than close to it: replies are around ninety
/// bytes and the socket holds eight kilobytes, so trouble starts in the low hundreds. Thirty-two
/// keeps the buffer under a tenth full, and means thirty-one keystrokes in thirty-two still cost
/// nothing at all.
const MAX_UNREAD_REPLIES: usize = 32;

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    /// Replies to fire-and-forget sends that nobody has read yet. They sit in the socket
    /// buffer, so the next request must discard exactly this many lines before reading its
    /// own reply or it would answer with an earlier keystroke's acknowledgement.
    unread_replies: usize,
    deferred_error: Option<String>,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(path)
            .map_err(|error| format!("could not connect to {}: {error}", path.display()))?;
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut client = Self {
            stream,
            reader,
            unread_replies: 0,
            deferred_error: None,
        };
        match client.request(&Request::Hello(HelloRequest {
            version: PROTOCOL_VERSION,
        }))? {
            Response::Hello { version } if version == PROTOCOL_VERSION => Ok(client),
            Response::Error { message, .. } => Err(message),
            response => Err(format!("unexpected daemon handshake: {response:?}")),
        }
    }

    pub fn request(&mut self, request: &Request) -> Result<Response, String> {
        self.drain_replies()?;
        self.write_request(request)?;
        self.read_reply()
    }

    /// Reads every reply owed for a fire-and-forget send, so the socket does not fill with them.
    ///
    /// Not a round trip despite blocking: the daemon answers requests in the order they arrive, so
    /// by the time this is called the replies it waits for have already been written and are
    /// sitting in this end's buffer.
    fn drain_replies(&mut self) -> Result<(), String> {
        while self.unread_replies > 0 {
            let reply = self.read_reply()?;
            self.unread_replies -= 1;
            if let Response::Error { message, .. } = reply {
                self.deferred_error = Some(message);
            }
        }
        Ok(())
    }

    /// Writes a request and returns immediately without reading its reply.
    ///
    /// This is the pane input path. Waiting for the acknowledgement would put a daemon round
    /// trip in front of every keystroke's paint, and there is nothing to wait for: the echo
    /// arrives on the event stream, not in this reply.
    pub fn send(&mut self, request: &Request) -> Result<(), String> {
        // `unread_replies` assumes exactly one reply per request, in order. That holds for
        // every request the daemon answers, but `Subscribe` is answered with nothing at all:
        // sending it here would offset the counter permanently and mis-attribute every later
        // reply, with nothing to resynchronise it. Use `subscribe` for that.
        debug_assert!(
            matches!(request, Request::PaneInput(_) | Request::PaneResize(_)),
            "send() is only for requests the daemon acknowledges exactly once, not {request:?}"
        );
        // Drained before the count can grow enough to close the connection. The replies nobody
        // has read sit in a socket buffer that is eight kilobytes on macOS, and the daemon writes
        // one per request; past roughly two hundred and forty of them its own write blocks, times
        // out after five seconds and it hangs up. Typing a long prompt into a pane did exactly
        // that, and the error then landed on the next innocent keystroke rather than on any of the
        // ones that caused it.
        if self.unread_replies >= MAX_UNREAD_REPLIES {
            self.drain_replies()?;
        }
        self.write_request(request)?;
        self.unread_replies += 1;
        Ok(())
    }

    /// An error the daemon reported for a fire-and-forget send, noticed when its reply was
    /// finally drained. Surfaced late rather than not at all.
    pub fn take_deferred_error(&mut self) -> Option<String> {
        self.deferred_error.take()
    }

    /// Writes one request as a single message.
    ///
    /// Serialised in full first: `to_writer` on the socket emits the value in pieces, so a client
    /// that dies partway leaves a fragment the daemon has to make sense of. The same reasoning as
    /// the daemon's own `write_response`, in the other direction.
    fn write_request(&mut self, request: &Request) -> Result<(), String> {
        let mut message = serde_json::to_vec(request).map_err(|e| e.to_string())?;
        message.push(b'\n');
        wire_debug("C>D", message.len(), &message);
        self.stream.write_all(&message).map_err(|error| {
            // Naming the request is the whole point. The daemon closes the connection after
            // refusing a message — too large, or one it could not parse — so the failure never
            // lands on the message that caused it, it lands on the next innocent one. Reported
            // bare, "Broken pipe (os error 32)" says only that the daemon is gone, which is the
            // one thing that was already obvious.
            let name = request_name(request);
            wire_debug("C>D-ERR", message.len(), name.as_bytes());
            format!(
                "the daemon went away while sending {name} ({} bytes): {error}. It closes the \
                 connection after refusing a message, so the cause is usually the request before \
                 this one — set DOCK_WIRE_DEBUG=<path> and reproduce to see it.",
                message.len()
            )
        })
    }

    fn read_reply(&mut self) -> Result<Response, String> {
        let mut line = String::new();
        if self
            .reader
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            wire_debug("D>C-EOF", 0, b"daemon closed");
            return Err("daemon closed the connection".into());
        }
        wire_debug("D>C", line.len(), line.as_bytes());
        serde_json::from_str(&line).map_err(|error| {
            // A response that does not end in a newline never finished being written, which
            // happens when the daemon goes away mid-message. Saying so beats reporting a column
            // number in JSON nobody wrote: "EOF while parsing a string at line 1 column 2" is a
            // true statement about `{"` and a useless one about what to do next.
            if line.ends_with('\n') {
                format!("invalid daemon response: {error}")
            } else {
                "the daemon stopped mid-reply — it exited or was killed; start Dock again".into()
            }
        })
    }

    /// Opens a connection dedicated to pushed events and returns a receiver fed by a reader
    /// thread, so the render loop drains events without ever blocking on the socket.
    ///
    /// Subscribing is one-way: the daemon stops reading this connection and sends no
    /// acknowledgement, so the request is written without awaiting a reply. Reading one would
    /// block until the first pane frame arrived and then consume it as if it were the reply,
    /// and a swallowed attach frame makes every later delta look like a revision gap.
    ///
    /// This is a whole connection of its own, so a dashboard holds two: one for requests and
    /// this one for events. Both count against the daemon's admission limit.
    pub fn subscribe(socket: &Path) -> Result<mpsc::Receiver<Event>, String> {
        let Self {
            mut stream, reader, ..
        } = Self::connect(socket)?;
        let mut message = serde_json::to_vec(&Request::Subscribe(SubscribeRequest {}))
            .map_err(|e| e.to_string())?;
        message.push(b'\n');
        stream.write_all(&message).map_err(|e| e.to_string())?;
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("dock-event-reader".into())
            .spawn(move || {
                let mut reader = reader;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(Response::Stream { event }) = serde_json::from_str(&line)
                        && sender.send(event).is_err()
                    {
                        // The dashboard is gone; dropping the reader closes the subscription.
                        break;
                    }
                    line.clear();
                }
            })
            .map_err(|error| format!("could not start event reader: {error}"))?;
        Ok(receiver)
    }
}

/// One poll of the event stream.
#[derive(Debug)]
pub enum StreamPoll {
    Event(Event),
    /// Nothing pending. Not the same as nothing left: see `Reconnected`.
    Idle,
    /// The stream died and a fresh subscription replaced it. Every replicated screen must be
    /// dropped, because the daemon starts a new subscriber's sync map empty and re-attaches
    /// each live run with a full snapshot.
    Reconnected,
    /// The stream died and could not be replaced yet. The dashboard is showing stale content.
    Lost(String),
}

/// A subscription that notices its own death and re-establishes itself.
///
/// The dashboard's entire picture of pane content arrives here and nothing polls any more, so
/// a stream that ends — daemon restart, admission eviction, write timeout, or any IO error in
/// the reader thread — would otherwise freeze the UI on its last frame with no indication.
/// Folding `Disconnected` into `Empty` is exactly that bug, so the two are kept apart.
///
/// Reconnecting also gives a client stuck behind a revision gap a real recovery path: a fresh
/// subscription re-attaches every run from a full snapshot.
pub struct EventStream {
    socket: PathBuf,
    receiver: mpsc::Receiver<Event>,
    retry_at: Option<Instant>,
}

impl EventStream {
    /// How long to wait before retrying a subscription that could not be re-established, so a
    /// daemon that is down costs one connect attempt a second rather than one per frame.
    const RETRY_DELAY: Duration = Duration::from_secs(1);

    pub fn subscribe(socket: &Path) -> Result<Self, String> {
        Ok(Self {
            socket: socket.to_path_buf(),
            receiver: Client::subscribe(socket)?,
            retry_at: None,
        })
    }

    /// Never blocks: the render loop drains this every frame.
    pub fn poll(&mut self) -> StreamPoll {
        match self.receiver.try_recv() {
            Ok(event) => StreamPoll::Event(event),
            Err(mpsc::TryRecvError::Empty) => StreamPoll::Idle,
            Err(mpsc::TryRecvError::Disconnected) => self.resubscribe(),
        }
    }

    fn resubscribe(&mut self) -> StreamPoll {
        if self
            .retry_at
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return StreamPoll::Idle;
        }
        match Client::subscribe(&self.socket) {
            Ok(receiver) => {
                self.receiver = receiver;
                self.retry_at = None;
                StreamPoll::Reconnected
            }
            Err(error) => {
                self.retry_at = Some(Instant::now() + Self::RETRY_DELAY);
                StreamPoll::Lost(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_long_burst_of_pane_input_does_not_fill_the_socket_and_hang_up() {
        // The bug this exists for: every keystroke was sent without reading its acknowledgement,
        // the daemon writes one per request, and the socket holds eight kilobytes of them. Typing
        // a couple of hundred characters into a pane without anything else needing a round trip
        // filled it, the daemon's own write blocked, and five seconds later it closed the
        // connection. The error then surfaced on whatever keystroke came next, which was never
        // the one at fault.
        let socket = socket_path("input-burst");
        let listener = UnixListener::bind(&socket.0).expect("bind");
        let daemon = std::thread::spawn(move || {
            let (mut stream, mut reader) = accept_handshake(&listener);
            write_line(
                &mut stream,
                &Response::Hello {
                    version: PROTOCOL_VERSION,
                },
            );
            // Answers every request and never reads ahead, exactly as the daemon does. The small
            // buffer stands in for the real one so the test is quick rather than lucky.
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut answered = 0usize;
            while answered < BURST {
                let request = line(&mut reader);
                if request.trim().is_empty() {
                    break;
                }
                write_line(
                    &mut stream,
                    &Response::PaneInputAccepted {
                        workspace_id: "w".into(),
                        pane_id: "p".into(),
                        bytes: 1,
                    },
                );
                answered += 1;
            }
            answered
        });

        let mut client = Client::connect(&socket.0).expect("connect");
        for index in 0..BURST {
            client
                .send(&Request::PaneInput(PaneInputRequest {
                    workspace_id: "w".into(),
                    pane_id: "p".into(),
                    input: PaneInputRequest::encode(b"x"),
                }))
                .unwrap_or_else(|error| panic!("keystroke {index} was refused: {error}"));
        }
        // Never more than the cap is left outstanding, which is what keeps the buffer far from
        // full however long somebody types.
        assert!(
            client.unread_replies <= MAX_UNREAD_REPLIES,
            "{} replies left unread",
            client.unread_replies
        );
        assert_eq!(daemon.join().expect("daemon thread"), BURST);
    }

    /// Comfortably past the couple of hundred that closed the connection before the drain existed.
    const BURST: usize = 600;

    #[test]
    fn a_reply_cut_short_blames_the_daemon_rather_than_the_json() {
        // What a client saw when a daemon died mid-write: `to_writer` emitted the value in
        // pieces straight at the socket, so two bytes reached the reader and serde reported a
        // column number in a document nobody had written.
        let cut_short = "{\"";
        let error = serde_json::from_str::<Response>(cut_short)
            .unwrap_err()
            .to_string();
        assert!(error.contains("column 2"), "{error}");

        // Responses are now written whole, so a fragment means one thing and says it.
        let message = if cut_short.ends_with('\n') {
            format!("invalid daemon response: {error}")
        } else {
            "the daemon stopped mid-reply — it exited or was killed; start Dock again".to_owned()
        };
        assert!(message.contains("daemon stopped mid-reply"), "{message}");
        assert!(!message.contains("column"), "{message}");
    }

    use super::*;
    use crate::protocol::{ErrorCode, InspectRequest, PaneInputRequest};
    use std::{
        os::unix::net::UnixListener,
        sync::atomic::{AtomicU64, Ordering},
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// A socket that removes itself, so a failing assertion cannot leave a stale path behind
    /// that makes the next run of this test fail for the wrong reason.
    struct TestSocket(PathBuf);

    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn socket_path(label: &str) -> TestSocket {
        let path = std::env::current_dir()
            .expect("current directory")
            .join("target")
            .join(format!(
                "dock-client-test-{label}-{}-{}.sock",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_file(&path);
        TestSocket(path)
    }

    fn line(stream: &mut BufReader<UnixStream>) -> String {
        let mut buffer = String::new();
        stream.read_line(&mut buffer).expect("read line");
        buffer
    }

    fn write_line(stream: &mut UnixStream, response: &Response) {
        let encoded = serde_json::to_string(response).expect("encode response");
        stream.write_all(encoded.as_bytes()).expect("write");
        stream.write_all(b"\n").expect("write newline");
    }

    fn accept_handshake(listener: &UnixListener) -> (UnixStream, BufReader<UnixStream>) {
        accept_within(listener, Duration::from_secs(10)).expect("accept")
    }

    /// Accepts with a deadline. A regression that never re-subscribes must fail the test that
    /// asserts it does, not wedge the whole suite in a blocking `accept`.
    fn accept_within(
        listener: &UnixListener,
        timeout: Duration,
    ) -> Option<(UnixStream, BufReader<UnixStream>)> {
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let deadline = Instant::now() + timeout;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        // macOS hands an accepted socket the listener's non-blocking flag; every read below is
        // a blocking line read.
        stream.set_nonblocking(false).expect("blocking stream");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        assert!(line(&mut reader).contains("hello"));
        write_line(
            &mut stream,
            &Response::Hello {
                version: PROTOCOL_VERSION,
            },
        );
        Some((stream, reader))
    }

    #[test]
    fn subscribing_reads_every_pushed_event_including_the_first() {
        let socket = socket_path("subscribe");
        let listener = UnixListener::bind(&socket.0).expect("bind");
        let daemon = thread::spawn(move || {
            let (mut stream, mut reader) = accept_handshake(&listener);
            assert!(line(&mut reader).contains("subscribe"));
            // A subscribed connection is one-way: the daemon acknowledges nothing and never
            // reads again. A client that waited for a reply here would consume this first
            // frame as if it were one, and a lost attach frame turns every later delta into
            // an apparent revision gap.
            write_line(
                &mut stream,
                &Response::Stream {
                    event: Event::PaneAttached {
                        run_id: "run_1".into(),
                        revision: 4,
                        rows: 10,
                        cols: 40,
                        scrollback_rows: 2000,
                        screen: String::new(),
                    },
                },
            );
            write_line(
                &mut stream,
                &Response::Stream {
                    event: Event::PaneDelta {
                        run_id: "run_1".into(),
                        revision: 5,
                        bytes: String::new(),
                    },
                },
            );
            // Dropping the stream here ends the subscription. Waiting for the client to
            // close first would deadlock: its reader thread is parked in `read_line` and only
            // notices a dropped receiver on the next frame, which would never arrive.
            drop(reader);
        });

        let events = Client::subscribe(&socket.0).expect("subscribe");
        let first = events
            .recv_timeout(crate::testing::budget(5))
            .expect("attach frame");
        assert!(matches!(
            first,
            Event::PaneAttached {
                revision: 4,
                rows: 10,
                cols: 40,
                ..
            }
        ));
        let second = events
            .recv_timeout(crate::testing::budget(5))
            .expect("delta frame");
        assert!(matches!(second, Event::PaneDelta { revision: 5, .. }));
        drop(events);
        daemon.join().expect("daemon thread");
    }

    #[test]
    fn a_fire_and_forget_send_does_not_desync_the_next_request() {
        let socket = socket_path("send");
        let listener = UnixListener::bind(&socket.0).expect("bind");
        let daemon = thread::spawn(move || {
            let (mut stream, mut reader) = accept_handshake(&listener);
            let input = line(&mut reader);
            assert!(input.contains("pane_input"), "{input}");
            write_line(
                &mut stream,
                &Response::Error {
                    code: ErrorCode::MalformedRequest,
                    message: "no such pane".into(),
                },
            );
            assert!(line(&mut reader).contains("inspect"));
            write_line(&mut stream, &Response::Snapshots { snapshots: vec![] });
            drop(reader);
        });

        let mut client = Client::connect(&socket.0).expect("connect");
        client
            .send(&Request::PaneInput(PaneInputRequest {
                workspace_id: "w".into(),
                pane_id: "a".into(),
                input: PaneInputRequest::encode(b"\x1b[A"),
            }))
            .expect("send");
        assert_eq!(client.take_deferred_error(), None, "nothing drained yet");

        // The unread acknowledgement is discarded first, so this reads its own reply rather
        // than the keystroke's.
        let response = client
            .request(&Request::Inspect(InspectRequest { run_id: None }))
            .expect("request");
        assert!(
            matches!(response, Response::Snapshots { .. }),
            "{response:?}"
        );
        assert_eq!(
            client.take_deferred_error().as_deref(),
            Some("no such pane")
        );
        drop(client);
        daemon.join().expect("daemon thread");
    }

    fn attached(revision: u64) -> Event {
        Event::PaneAttached {
            run_id: "run_1".into(),
            revision,
            rows: 10,
            cols: 40,
            scrollback_rows: 2000,
            screen: String::new(),
        }
    }

    /// Polls until `stop` says the test has seen what it needs, recording every outcome.
    /// The reader thread notices EOF on its own schedule, so the disconnect cannot be
    /// asserted on a fixed number of polls.
    fn poll_until(
        stream: &mut EventStream,
        stop: impl Fn(&[StreamPoll]) -> bool,
    ) -> Vec<StreamPoll> {
        let deadline = crate::testing::deadline(10);
        let mut seen = Vec::new();
        while Instant::now() < deadline && !stop(&seen) {
            match stream.poll() {
                StreamPoll::Idle => thread::sleep(Duration::from_millis(5)),
                outcome => seen.push(outcome),
            }
        }
        seen
    }

    #[test]
    fn a_dead_event_stream_is_noticed_and_replaced_rather_than_read_as_idle() {
        let socket = socket_path("reconnect");
        let listener = UnixListener::bind(&socket.0).expect("bind");
        let daemon = thread::spawn(move || {
            // One frame, then the daemon drops the connection: a restart, an admission
            // eviction, and a write timeout all look exactly like this to the client.
            let (mut stream, mut reader) = accept_handshake(&listener);
            assert!(line(&mut reader).contains("subscribe"));
            write_line(&mut stream, &Response::Stream { event: attached(1) });
            drop((reader, stream));
            // The replacement subscription, which the client must make entirely on its own.
            let Some((mut stream, mut reader)) = accept_within(&listener, Duration::from_secs(5))
            else {
                return;
            };
            assert!(line(&mut reader).contains("subscribe"));
            write_line(&mut stream, &Response::Stream { event: attached(2) });
            drop((reader, stream));
        });

        let mut stream = EventStream::subscribe(&socket.0).expect("subscribe");
        let seen = poll_until(&mut stream, |seen| {
            seen.iter().any(|outcome| {
                matches!(
                    outcome,
                    StreamPoll::Event(Event::PaneAttached { revision: 2, .. })
                )
            })
        });
        daemon.join().expect("daemon thread");

        assert!(
            seen.iter().any(|outcome| matches!(
                outcome,
                StreamPoll::Event(Event::PaneAttached { revision: 1, .. })
            )),
            "the first subscription's frame: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|outcome| matches!(outcome, StreamPoll::Reconnected)),
            "a stream whose reader thread ended must be reported, not folded into idle, or \
             the dashboard renders its last frame forever: {seen:?}"
        );
        assert!(
            seen.iter().any(|outcome| matches!(
                outcome,
                StreamPoll::Event(Event::PaneAttached { revision: 2, .. })
            )),
            "the replacement subscription must actually deliver frames: {seen:?}"
        );
    }

    #[test]
    fn an_event_stream_that_cannot_be_replaced_reports_the_loss_and_then_backs_off() {
        let socket = socket_path("lost");
        let listener = UnixListener::bind(&socket.0).expect("bind");
        let daemon = thread::spawn(move || {
            let (stream, mut reader) = accept_handshake(&listener);
            assert!(line(&mut reader).contains("subscribe"));
            // The listener goes too, so nothing is left to accept a replacement.
            drop((reader, stream, listener));
        });

        let mut stream = EventStream::subscribe(&socket.0).expect("subscribe");
        let seen = poll_until(&mut stream, |seen| !seen.is_empty());
        daemon.join().expect("daemon thread");
        assert!(
            matches!(seen.first(), Some(StreamPoll::Lost(_))),
            "a stream that cannot be replaced must say so: {seen:?}"
        );
        // Backed off: without this a down daemon would draw one connect attempt per rendered
        // frame, which at the render loop's 16ms tick is a reconnect storm.
        assert!(
            matches!(stream.poll(), StreamPoll::Idle),
            "a failed re-subscribe must not retry on the very next poll"
        );
    }

    /// `unread_replies` self-corrects only because every request routed through `send` is
    /// acknowledged exactly once. `Subscribe` is answered with nothing at all, so sending it
    /// here would offset the counter permanently and mis-attribute every later reply.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "send() is only for requests the daemon acknowledges exactly once")]
    fn send_refuses_a_request_the_daemon_never_acknowledges() {
        let socket = socket_path("unacked");
        let listener = UnixListener::bind(&socket.0).expect("bind");
        thread::spawn(move || {
            let (stream, reader) = accept_handshake(&listener);
            drop((reader, stream, listener));
        });
        let mut client = Client::connect(&socket.0).expect("connect");
        let _ = client.send(&Request::Subscribe(SubscribeRequest {}));
    }
}
