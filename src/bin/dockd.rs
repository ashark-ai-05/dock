use std::{path::PathBuf, sync::Arc};

use dock::{
    dispatch::{CapacityPolicy, RuntimeRegistry},
    paths, server,
};

fn main() -> Result<(), String> {
    let args = std::env::args().skip(1);
    let mut socket: Option<PathBuf> = None;
    let mut capacity = 2000;
    let mut state_dir = PathBuf::from(".dock/local");
    let mut global_run_capacity = usize::MAX;
    let mut repository_run_capacity = usize::MAX;
    let mut human_review_reserved = 0;
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
        } else {
            return Err(format!(
                "unknown option {argument:?}; usage: dockd [--socket=PATH] [--state-dir=PATH] [--scrollback-rows=N] [--global-run-capacity=N] [--repository-run-capacity=N] [--human-review-reserved=N]"
            ));
        }
    }
    let socket = match socket {
        Some(socket) => socket,
        None => paths::prepare_default_socket_path()?,
    };
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
    eprintln!("dockd listening on {}", socket.display());
    server.serve(runtime)
}
