//! One running instance on macOS: a second launch hands its working directory and arguments to
//! the running app and exits.
//!
//! `tauri-plugin-single-instance` meets the running instance at a fixed
//! `/tmp/<identifier>_si.sock` on macOS, in a directory every local user shares, and hands its
//! cwd and arguments to whatever listens there before exiting. Another local user could listen
//! first, stop Oxbase from starting and read its launch arguments (audit lead 17). Here the
//! socket lives in a private directory of the user (the bundle identifier in the per-user
//! `$TMPDIR`, created `0o700` and refused when it is a link, another user's or open to others;
//! `oxbase_types::private_fs`), and both ends check that the peer runs as the same user
//! (`getpeereid`, through tokio's `peer_cred`) before anything is sent or accepted. A stale
//! socket, a foreign peer or an unusable directory never blocks startup: the app then starts on
//! its own.
//!
//! Linux (a D-Bus name on the per-user session bus) and Windows (a session-local named mutex)
//! keep using the plugin.

use std::io;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Largest hand-over accepted (a working directory and a command line).
const MAX_MESSAGE: u64 = 1024 * 1024;

/// The socket in the private directory `dir_name` under `base`, creating the directory
/// (`0o700`) when needed. Fails when it is not private to the user.
pub(crate) fn socket_path(base: &Path, dir_name: &str) -> io::Result<PathBuf> {
    let dir = base.join(dir_name);
    oxbase_types::private_fs::create_private_dir_all(&dir)?;
    Ok(dir.join("si.sock"))
}

/// Fails unless the process at the other end of `stream` runs as this process's user.
fn same_user(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() == oxbase_types::private_fs::effective_uid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the single-instance peer runs as another user",
        ))
    }
}

/// Hands `message` to the instance listening at `socket`. `NotFound` / `ConnectionRefused`: none
/// listens. `PermissionDenied`: the listener is another user's, and nothing was sent.
pub(crate) async fn notify(socket: &Path, message: &[u8]) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket).await?;
    same_user(&stream)?;
    stream.write_all(message).await?;
    stream.shutdown().await
}

/// Reads one hand-over from an accepted connection, refusing (unread) one from another user.
pub(crate) async fn receive(stream: UnixStream) -> io::Result<Vec<u8>> {
    same_user(&stream)?;
    let mut message = Vec::new();
    stream.take(MAX_MESSAGE).read_to_end(&mut message).await?;
    Ok(message)
}

/// `cwd`, two NULs, then the arguments separated by NULs.
pub(crate) fn encode(cwd: &str, args: &[String]) -> Vec<u8> {
    let mut message = format!("{cwd}\0\0");
    message.push_str(&args.join("\0"));
    message.into_bytes()
}

/// The arguments and working directory of an [`encode`]d hand-over.
pub(crate) fn decode(message: &[u8]) -> (Vec<String>, String) {
    let text = String::from_utf8_lossy(message);
    let (cwd, args) = text.split_once("\0\0").unwrap_or_default();
    (args.split('\0').map(String::from).collect(), cwd.to_owned())
}

#[cfg(target_os = "macos")]
pub use plugin::init;

#[cfg(target_os = "macos")]
mod plugin {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tauri::plugin::TauriPlugin;
    use tauri::{AppHandle, Manager, RunEvent, Runtime};

    use super::{decode, encode, notify, receive, socket_path};

    /// The socket this instance listens on (removed on exit).
    struct Listening(PathBuf);

    /// The plugin: hands over and exits when another instance of the user runs, else listens
    /// and calls `callback` with each later launch's arguments and working directory.
    pub fn init<R, F>(callback: F) -> TauriPlugin<R>
    where
        R: Runtime,
        F: Fn(&AppHandle<R>, Vec<String>, String) + Send + Sync + 'static,
    {
        let callback = Arc::new(callback);
        tauri::plugin::Builder::new("oxbase-single-instance")
            .setup(move |app, _api| {
                let Ok(socket) = socket_path(&std::env::temp_dir(), &app.config().identifier)
                else {
                    return Ok(());
                };
                let cwd = std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let args: Vec<String> = std::env::args_os()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect();
                let message = encode(&cwd, &args);
                match tauri::async_runtime::block_on(notify(&socket, &message)) {
                    Ok(()) => std::process::exit(0),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        // A stale socket of an earlier instance of this user.
                        let _ = std::fs::remove_file(&socket);
                        listen(app.clone(), socket, callback);
                    }
                    // Not this user's instance, or the socket is unusable: start alone.
                    Err(_) => {}
                }
                Ok(())
            })
            .on_event(|app, event| {
                if let RunEvent::Exit = event
                    && let Some(socket) = app.try_state::<Listening>()
                {
                    let _ = std::fs::remove_file(&socket.0);
                }
            })
            .build()
    }

    fn listen<R, F>(app: AppHandle<R>, socket: PathBuf, callback: Arc<F>)
    where
        R: Runtime,
        F: Fn(&AppHandle<R>, Vec<String>, String) + Send + Sync + 'static,
    {
        app.manage(Listening(socket.clone()));
        tauri::async_runtime::spawn(async move {
            let Ok(listener) = tokio::net::UnixListener::bind(&socket) else {
                return;
            };
            while let Ok((stream, _)) = listener.accept().await {
                let app = app.clone();
                let callback = Arc::clone(&callback);
                tauri::async_runtime::spawn(async move {
                    if let Ok(message) = receive(stream).await {
                        let (args, cwd) = decode(&message);
                        callback(&app, args, cwd);
                    }
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    #[test]
    fn hand_overs_round_trip() {
        let args = vec!["/app/Oxbase".to_owned(), "/tmp/dummy.sql".to_owned()];
        let message = encode("/home/u", &args);
        assert_eq!(message, b"/home/u\0\0/app/Oxbase\0/tmp/dummy.sql");
        assert_eq!(decode(&message), (args, "/home/u".to_owned()));
    }

    #[test]
    fn the_socket_directory_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = socket_path(tmp.path(), "dev.oxbase.app").unwrap();
        let dir = socket.parent().unwrap();
        assert_eq!(dir, tmp.path().join("dev.oxbase.app"));
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // A directory open to others (or a link, or another user's) is refused.
        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(socket_path(tmp.path(), "open").is_err());
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(dir, &link).unwrap();
        assert!(socket_path(tmp.path(), "link").is_err());
    }

    #[tokio::test]
    async fn a_running_instance_of_the_same_user_gets_the_hand_over() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = socket_path(tmp.path(), "dev.oxbase.app").unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let message = encode("/home/u", &["/app/Oxbase".to_owned()]);
        let (sent, received) = tokio::join!(notify(&socket, &message), async {
            let (stream, _) = listener.accept().await.unwrap();
            receive(stream).await
        });
        sent.unwrap();
        assert_eq!(received.unwrap(), message);
    }

    #[tokio::test]
    async fn no_instance_or_a_stale_socket_means_starting_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = socket_path(tmp.path(), "dev.oxbase.app").unwrap();
        let err = notify(&socket, b"x").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        // The socket file of an instance that is gone.
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        let err = notify(&socket, b"x").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }
}
