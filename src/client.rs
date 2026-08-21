use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
};

use crate::protocol::{HelloRequest, PROTOCOL_VERSION, Request, Response};

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(path)
            .map_err(|error| format!("could not connect to {}: {error}", path.display()))?;
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut client = Self { stream, reader };
        match client.request(&Request::Hello(HelloRequest {
            version: PROTOCOL_VERSION,
        }))? {
            Response::Hello { version } if version == PROTOCOL_VERSION => Ok(client),
            Response::Error { message, .. } => Err(message),
            response => Err(format!("unexpected daemon handshake: {response:?}")),
        }
    }

    pub fn request(&mut self, request: &Request) -> Result<Response, String> {
        serde_json::to_writer(&mut self.stream, request).map_err(|e| e.to_string())?;
        self.stream.write_all(b"\n").map_err(|e| e.to_string())?;
        let mut line = String::new();
        if self
            .reader
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            return Err("daemon closed the connection".into());
        }
        serde_json::from_str(&line).map_err(|e| format!("invalid daemon response: {e}"))
    }
}
