use std::{path::PathBuf, sync::Arc};

use dock::{paths, runtime::OwnedRuntime, server};

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut socket: Option<PathBuf> = None;
    let mut capacity = 64 * 1024;
    let mut command = Vec::new();
    while let Some(argument) = args.next() {
        if argument == "--" {
            command.extend(args);
            break;
        } else if let Some(value) = argument.strip_prefix("--socket=") {
            socket = Some(value.into());
        } else if let Some(value) = argument.strip_prefix("--scrollback-bytes=") {
            capacity = value
                .parse()
                .map_err(|_| "--scrollback-bytes must be a positive integer")?;
            if capacity == 0 {
                return Err("--scrollback-bytes must be greater than zero".into());
            }
        } else {
            return Err(format!(
                "unknown option {argument:?}; usage: dockd [--socket=PATH] [--scrollback-bytes=N] -- COMMAND [ARG ...]"
            ));
        }
    }
    if command.is_empty() {
        return Err("fixture command is required after --".into());
    }
    let socket = match socket {
        Some(socket) => socket,
        None => paths::prepare_default_socket_path()?,
    };
    let server = server::Server::bind(&socket)?;
    let runtime = Arc::new(OwnedRuntime::launch(command, capacity));
    eprintln!("dockd listening on {}", socket.display());
    server.serve(runtime)
}
