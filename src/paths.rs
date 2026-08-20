use std::{
    fs,
    io::ErrorKind,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
};

const RUNTIME_DIRECTORY_MODE: u32 = 0o700;

pub fn default_socket_path() -> Result<PathBuf, String> {
    let uid = unsafe { nix::libc::geteuid() };
    #[cfg(target_os = "linux")]
    let (base, directory) = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(path) => (PathBuf::from(path), "dock".to_owned()),
        None => (std::env::temp_dir(), format!("dock-{uid}")),
    };
    #[cfg(target_os = "macos")]
    let (base, directory) = (std::env::temp_dir(), format!("dock-{uid}"));
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let (base, directory) = (std::env::temp_dir(), format!("dock-{uid}"));

    socket_path_in(&base, &directory, uid)
}

pub fn prepare_default_socket_path() -> Result<PathBuf, String> {
    let socket = default_socket_path()?;
    let uid = unsafe { nix::libc::geteuid() };
    recover_stale_socket(
        &socket,
        socket.parent().expect("default socket has parent"),
        uid,
    )?;
    Ok(socket)
}

fn recover_stale_socket(socket: &Path, runtime_directory: &Path, uid: u32) -> Result<(), String> {
    if socket.parent() != Some(runtime_directory) {
        return Err(format!(
            "refusing stale-socket recovery outside Dock runtime directory: {}",
            socket.display()
        ));
    }

    let directory = fs::symlink_metadata(runtime_directory)
        .map_err(|error| format!("could not inspect runtime directory: {error}"))?;
    if !directory.is_dir()
        || directory.file_type().is_symlink()
        || directory.uid() != uid
        || directory.permissions().mode() & 0o077 != 0
    {
        return Err(format!(
            "refusing stale-socket recovery in untrusted runtime directory: {}",
            runtime_directory.display()
        ));
    }

    let metadata = match fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not inspect socket path: {error}")),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(format!(
            "refusing to remove untrusted socket path: {}",
            socket.display()
        ));
    }

    match UnixStream::connect(socket) {
        Ok(_) => Err(format!("socket path is already live: {}", socket.display())),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => fs::remove_file(socket)
            .map_err(|error| {
                format!(
                    "could not remove stale socket {}: {error}",
                    socket.display()
                )
            }),
        Err(error) => Err(format!(
            "refusing to remove socket {} after inconclusive connect probe: {error}",
            socket.display()
        )),
    }
}

fn socket_path_in(base: &Path, directory: &str, uid: u32) -> Result<PathBuf, String> {
    let runtime_directory = base.join(directory);
    match fs::symlink_metadata(&runtime_directory) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(format!(
                    "runtime path is not a directory: {}",
                    runtime_directory.display()
                ));
            }
            if metadata.uid() != uid {
                return Err(format!(
                    "runtime directory is not owned by the current user: {}",
                    runtime_directory.display()
                ));
            }
            fs::set_permissions(
                &runtime_directory,
                fs::Permissions::from_mode(RUNTIME_DIRECTORY_MODE),
            )
            .map_err(|error| format!("could not restrict runtime directory: {error}"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&runtime_directory).map_err(|error| {
                format!(
                    "could not create runtime directory {}: {error}",
                    runtime_directory.display()
                )
            })?;
            fs::set_permissions(
                &runtime_directory,
                fs::Permissions::from_mode(RUNTIME_DIRECTORY_MODE),
            )
            .map_err(|error| format!("could not restrict runtime directory: {error}"))?;
        }
        Err(error) => return Err(format!("could not inspect runtime directory: {error}")),
    }
    Ok(runtime_directory.join("dockd.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::net::UnixListener,
        sync::atomic::{AtomicU64, Ordering},
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn socket_helper_creates_an_owner_only_runtime_directory() {
        let base = std::env::current_dir()
            .expect("current directory")
            .join("target")
            .join(format!(
                "dock-path-test-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&base).expect("test base");
        let uid = unsafe { nix::libc::geteuid() };
        let socket = socket_path_in(&base, "runtime", uid).expect("runtime socket path");
        assert_eq!(socket, base.join("runtime/dockd.sock"));
        let metadata = fs::metadata(base.join("runtime")).expect("runtime metadata");
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        fs::remove_dir_all(base).expect("remove test base");
    }

    fn test_runtime() -> (PathBuf, PathBuf, u32) {
        // Keep this below macOS's short sockaddr_un.sun_path limit even when the checkout path is
        // long; PID + sequence retain deterministic per-process isolation.
        let base = std::env::temp_dir().join(format!(
            "dock-sock-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&base).expect("test base");
        let uid = unsafe { nix::libc::geteuid() };
        let socket = socket_path_in(&base, "runtime", uid).expect("runtime socket path");
        (base, socket, uid)
    }

    fn bind_test_socket(socket: &Path) -> Option<UnixListener> {
        match UnixListener::bind(socket) {
            Ok(listener) => Some(listener),
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                eprintln!("skipping Unix socket test: sandbox denied bind: {error}");
                None
            }
            Err(error) => panic!("bind socket fixture: {error}"),
        }
    }

    #[test]
    fn stale_owned_socket_in_runtime_directory_is_recovered() {
        let (base, socket, uid) = test_runtime();
        let Some(listener) = bind_test_socket(&socket) else {
            fs::remove_dir_all(base).expect("remove test base");
            return;
        };
        drop(listener);
        // macOS can briefly report a successful connect while the just-dropped listener's
        // kernel state is unwinding. A real stale socket is recoverable only after refusal.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !matches!(UnixStream::connect(&socket), Err(error) if error.kind() == ErrorKind::ConnectionRefused)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        recover_stale_socket(&socket, socket.parent().expect("socket parent"), uid)
            .expect("recover stale socket");

        assert!(!socket.exists());
        fs::remove_dir_all(base).expect("remove test base");
    }

    #[test]
    fn live_socket_is_refused_and_preserved() {
        let (base, socket, uid) = test_runtime();
        let Some(listener) = bind_test_socket(&socket) else {
            fs::remove_dir_all(base).expect("remove test base");
            return;
        };

        let error = recover_stale_socket(&socket, socket.parent().expect("socket parent"), uid)
            .expect_err("live socket must be refused");

        assert!(error.contains("already live"), "unexpected error: {error}");
        assert!(socket.exists());
        drop(listener);
        fs::remove_dir_all(base).expect("remove test base");
    }

    #[test]
    fn external_socket_override_is_refused_and_preserved() {
        let (base, default_socket, uid) = test_runtime();
        let external_directory = base.join("external");
        fs::create_dir(&external_directory).expect("external directory");
        fs::set_permissions(&external_directory, fs::Permissions::from_mode(0o700))
            .expect("restrict external directory");
        let external_socket = external_directory.join("override.sock");
        let Some(listener) = bind_test_socket(&external_socket) else {
            fs::remove_dir_all(base).expect("remove test base");
            return;
        };
        drop(listener);

        let error = recover_stale_socket(
            &external_socket,
            default_socket.parent().expect("default socket parent"),
            uid,
        )
        .expect_err("external override must be refused");

        assert!(error.contains("outside Dock runtime directory"));
        assert!(external_socket.exists());
        fs::remove_dir_all(base).expect("remove test base");
    }
}
