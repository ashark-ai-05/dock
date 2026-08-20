use std::{path::PathBuf, sync::Arc};

use dock::{dispatch::RuntimeRegistry, paths, server};

fn main() -> Result<(), String> {
    let args = std::env::args().skip(1);
    let mut socket: Option<PathBuf> = None;
    let mut capacity = 64 * 1024;
    let mut state_dir = PathBuf::from(".dock/local");
    for argument in args {
        if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(value.into());
        } else if let Some(value) = argument.strip_prefix("--state-dir=") {
            state_dir = value.into();
        } else if let Some(value) = argument.strip_prefix("--scrollback-bytes=") {
            capacity = value
                .parse()
                .map_err(|_| "--scrollback-bytes must be a positive integer")?;
            if capacity == 0 {
                return Err("--scrollback-bytes must be greater than zero".into());
            }
        } else {
            return Err(format!(
                "unknown option {argument:?}; usage: dockd [--socket=PATH] [--state-dir=PATH] [--scrollback-bytes=N]"
            ));
        }
    }
    let socket = match socket {
        Some(socket) => socket,
        None => paths::prepare_default_socket_path()?,
    };
    let server = server::Server::bind(&socket)?;
    let runtime = Arc::new(RuntimeRegistry::new(state_dir, capacity)?);
    eprintln!("dockd listening on {}", socket.display());
    server.serve(runtime)
}
