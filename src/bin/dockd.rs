use std::{path::PathBuf, sync::Arc};

use dock::{
    dispatch::{CapacityPolicy, RuntimeRegistry},
    paths,
    queue::AutoFeedTrust,
    server,
};

fn main() -> Result<(), String> {
    let args = std::env::args().skip(1);
    let mut socket: Option<PathBuf> = None;
    let mut capacity = 2000;
    let mut state_dir = PathBuf::from(".dock/local");
    let mut global_run_capacity = usize::MAX;
    let mut repository_run_capacity = usize::MAX;
    let mut human_review_reserved = 0;
    // Which "the agent finished" signal an *already-armed* pane believes. It arms nothing: there
    // is deliberately no option here, or anywhere else, that turns auto-feed on by itself.
    let mut auto_feed_trust = AutoFeedTrust::Reported;
    for argument in args {
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(value.into());
        } else if let Some(value) = argument.strip_prefix("--state-dir=") {
            state_dir = value.into();
        } else if let Some(value) = argument.strip_prefix("--scrollback-rows=") {
            capacity = value
                .parse()
                .map_err(|_| "--scrollback-rows must be a positive integer")?;
            if capacity == 0 {
                return Err("--scrollback-rows must be greater than zero".into());
            }
        } else if let Some(value) = argument.strip_prefix("--global-run-capacity=") {
            global_run_capacity = value
                .parse()
                .map_err(|_| "--global-run-capacity must be a positive integer")?;
        } else if let Some(value) = argument.strip_prefix("--repository-run-capacity=") {
            repository_run_capacity = value
                .parse()
                .map_err(|_| "--repository-run-capacity must be a positive integer")?;
        } else if let Some(value) = argument.strip_prefix("--human-review-reserved=") {
            human_review_reserved = value
                .parse()
                .map_err(|_| "--human-review-reserved must be a non-negative integer")?;
        } else if let Some(value) = argument.strip_prefix("--auto-feed-trust=") {
            auto_feed_trust = match value {
                "reported" => AutoFeedTrust::Reported,
                "screen" => AutoFeedTrust::Screen,
                _ => return Err("--auto-feed-trust must be reported or screen".into()),
            };
        } else {
            return Err(format!(
                "unknown option {argument:?}; usage: dockd [--socket=PATH] [--state-dir=PATH] [--scrollback-rows=N] [--global-run-capacity=N] [--repository-run-capacity=N] [--human-review-reserved=N] [--auto-feed-trust=reported|screen]"
            ));
        }
    }
    let socket = match socket {
        Some(socket) => socket,
        None => paths::prepare_default_socket_path()?,
    };
    // Handed to every pane the daemon launches, so an agent can file a result without being told
    // where to find the daemon that started it.
    // SAFETY: single-threaded startup, before the server or any runtime thread exists.
    unsafe { std::env::set_var("DOCK_SOCKET_PATH", &socket) };
    let server = server::Server::bind(&socket)?;
    let runtime = Arc::new(RuntimeRegistry::with_capacity(
        state_dir,
        capacity,
        CapacityPolicy {
            global_run_capacity,
            per_repository_run_capacity: repository_run_capacity,
            human_review_reserved,
        },
    )?);
    runtime.set_auto_feed_trust(auto_feed_trust);
    // A pane restored from durable layout comes back with no run at all. `dock` auto-starts
    // `dockd`, so without this every pane is inert after a reboot.
    runtime.revive_restored_panes();
    eprintln!("dockd listening on {}", socket.display());
    server.serve(runtime)
}
