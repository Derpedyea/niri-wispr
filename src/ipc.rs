use anyhow::{Context, Result};
use socket2::{Domain, SockAddr, Socket, Type};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const SERVER_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_FRAME_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyCommand {
    Start,
    Stop,
    Cancel,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Start,
    Stop,
    Cancel,
    Quit,
    /// Open the settings window.
    Settings,
    /// Re-read config.toml and apply changes (restarts the hotkey watcher if needed).
    Reload,
    /// Internal events are tied to the watcher that produced them.
    Hotkey {
        generation: u64,
        action: HotkeyCommand,
    },
}

impl From<HotkeyCommand> for Command {
    fn from(action: HotkeyCommand) -> Self {
        match action {
            HotkeyCommand::Start => Self::Start,
            HotkeyCommand::Stop => Self::Stop,
            HotkeyCommand::Cancel => Self::Cancel,
            HotkeyCommand::Toggle => Self::Toggle,
        }
    }
}

impl Command {
    fn parse(s: &str) -> Option<Command> {
        match s.trim() {
            "toggle" => Some(Command::Toggle),
            "start" => Some(Command::Start),
            "stop" => Some(Command::Stop),
            "cancel" => Some(Command::Cancel),
            "quit" => Some(Command::Quit),
            "settings" => Some(Command::Settings),
            "reload" => Some(Command::Reload),
            _ => None,
        }
    }

    fn as_str(&self) -> Result<&'static str> {
        match self {
            Command::Toggle => Ok("toggle"),
            Command::Start => Ok("start"),
            Command::Stop => Ok("stop"),
            Command::Cancel => Ok("cancel"),
            Command::Quit => Ok("quit"),
            Command::Settings => Ok("settings"),
            Command::Reload => Ok("reload"),
            Command::Hotkey { .. } => anyhow::bail!("hotkey events cannot be sent over IPC"),
        }
    }
}

pub fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!(
        "dictationapp-{}.sock",
        std::env::var("USER").unwrap_or_else(|_| "unknown".into())
    ))
}

/// Return only after the command has reached the running instance's queue.
/// The acknowledgement keeps consecutive CLI invocations in the same order.
pub fn send(cmd: Command) -> Result<()> {
    send_to(&socket_path(), cmd)
}

fn send_to(path: &Path, cmd: Command) -> Result<()> {
    let command = cmd.as_str()?;
    let mut stream = connect_bounded(path, CLIENT_TIMEOUT)
        .context("could not reach dictationapp — is it running?")?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    stream.write_all(command.as_bytes())?;
    stream.write_all(b"\n")?;
    let reply = read_frame(&mut stream, CLIENT_TIMEOUT)?;
    anyhow::ensure!(reply == b"ok", "dictationapp did not accept the command");
    Ok(())
}

fn connect_bounded(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    match socket.connect(&SockAddr::unix(path)?) {
        Ok(()) => {}
        // AF_UNIX returns EAGAIN for a full listener backlog. This is not an
        // in-progress connection: writable polling alone could report success.
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Err(error),
        Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "IPC connection timed out",
                ));
            }
            let mut descriptor = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let milliseconds = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
            // The descriptor remains owned by socket throughout this call.
            let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
            if result > 0 {
                if let Some(error) = socket.take_error()? {
                    return Err(error);
                }
                break;
            }
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        },
        Err(error) => return Err(error),
    }
    socket.peer_addr()?;
    socket.set_nonblocking(false)?;
    let descriptor: OwnedFd = socket.into();
    Ok(UnixStream::from(descriptor))
}

/// Owns the socket, the dispatcher and a lock held until shutdown completes.
/// The lock file stays on disk: unlinking it would let two processes lock
/// different inodes. A crash releases the OS lock, leaving a recoverable socket.
pub struct Server {
    path: PathBuf,
    _lock: InstanceLock,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

struct InstanceLock(File);

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit this open file description
        // before CLOEXEC runs. Closing our descriptor alone then retains the
        // lock in that child; explicit unlock releases it on every owner exit.
        // Server's fields drop only after its joined shutdown/socket cleanup.
        for attempt in 0..3 {
            match self.0.unlock() {
                Ok(()) => return,
                Err(error) if attempt == 2 => {
                    eprintln!("ipc: instance lock cleanup failed: {error}");
                }
                Err(_) => thread::yield_now(),
            }
        }
    }
}

impl Server {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            if worker.join().is_err() {
                eprintln!("ipc: dispatcher panicked during shutdown");
            }
        }
        if let Err(error) = remove_socket(&self.path) {
            // The next owner can retry stale-socket recovery under the same lock.
            eprintln!("ipc: socket cleanup failed: {error:#}");
        }
    }
}

pub fn listen(tx: Sender<Command>) -> Result<Server> {
    listen_at(socket_path(), tx, SERVER_TIMEOUT)
}

fn listen_at(path: PathBuf, tx: Sender<Command>, timeout: Duration) -> Result<Server> {
    let lock_path = path.with_extension("lock");
    let lock = InstanceLock(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("failed to open {}", lock_path.display()))?,
    );
    lock.0
        .try_lock()
        .context("could not acquire instance lock — is another dictationapp running?")?;

    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            match connect_bounded(&path, CLIENT_TIMEOUT) {
                Ok(_) => anyhow::bail!("another dictationapp is already running"),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    anyhow::ensure!(
                        std::fs::symlink_metadata(&path)?.file_type().is_socket(),
                        "refusing to remove a non-socket IPC path"
                    );
                    remove_socket(&path)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("could not check existing IPC socket"),
            }
            UnixListener::bind(&path)
                .with_context(|| format!("failed to bind {}", path.display()))?
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to bind {}", path.display()));
        }
    };
    let mut server = Server {
        path,
        _lock: lock,
        stopped: Arc::new(AtomicBool::new(false)),
        worker: None,
    };
    listener.set_nonblocking(true)?;
    let stopped = server.stopped.clone();
    server.worker = Some(
        thread::Builder::new()
            .name("dictation-ipc".into())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if let Err(error) = forward_command(&mut stream, &tx, timeout) {
                                eprintln!("ipc: rejected client: {error:#}");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::park_timeout(Duration::from_millis(20));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            eprintln!("ipc: accept failed: {error}");
                            thread::park_timeout(Duration::from_millis(20));
                        }
                    }
                }
            })
            .context("could not start IPC dispatcher")?,
    );
    Ok(server)
}

fn forward_command(stream: &mut UnixStream, tx: &Sender<Command>, timeout: Duration) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(timeout))?;
    let result = read_frame(stream, timeout).and_then(|frame| {
        let line = std::str::from_utf8(&frame).context("command is not UTF-8")?;
        let cmd = Command::parse(line).context("unknown command")?;
        tx.send(cmd).context("command queue is closed")?;
        eprintln!("ipc: sent {cmd:?}");
        Ok(())
    });
    stream.write_all(if result.is_ok() { b"ok\n" } else { b"error\n" })?;
    result
}

/// A total deadline and byte limit also bound clients that never send a newline
/// or drip bytes slowly. Each connection carries exactly one command.
fn read_frame(stream: &mut UnixStream, timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut frame = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "IPC read timed out");
        stream.set_read_timeout(Some(remaining))?;
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => anyhow::bail!("IPC connection closed before acknowledgement or command"),
            Ok(_) if byte[0] == b'\n' => return Ok(frame),
            Ok(_) => {
                anyhow::ensure!(frame.len() < MAX_FRAME_BYTES, "IPC frame is too long");
                frame.push(byte[0]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("IPC read failed"),
        }
    }
}

fn remove_socket(path: &Path) -> Result<()> {
    for attempt in 0..3 {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if attempt == 2 => {
                return Err(error).with_context(|| format!("could not remove {}", path.display()));
            }
            Err(_) => thread::yield_now(),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, TryRecvError};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = PathBuf::from(format!(
                "/tmp/dictation-ipc-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("socket")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn instance_lock_covers_stale_recovery_and_shutdown() {
        const STALE_SOCKET: &str = "DICTATION_TEST_STALE_SOCKET";
        if let Some(path) = std::env::var_os(STALE_SOCKET) {
            // This isolated process never forks while its listener is open.
            // Its exit proves no inherited descriptor keeps the socket live.
            drop(UnixListener::bind(PathBuf::from(path)).unwrap());
            return;
        }
        let dir = TestDir::new();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ipc::tests::instance_lock_covers_stale_recovery_and_shutdown",
                "--nocapture",
            ])
            .env(STALE_SOCKET, dir.socket())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let (tx, _rx) = mpsc::channel();
        let server = listen_at(dir.socket(), tx.clone(), SERVER_TIMEOUT).unwrap();
        let inode = std::fs::metadata(dir.socket().with_extension("lock"))
            .unwrap()
            .ino();
        assert!(listen_at(dir.socket(), tx.clone(), SERVER_TIMEOUT).is_err());
        drop(server);
        assert!(!dir.socket().exists());
        let restarted = listen_at(dir.socket(), tx, SERVER_TIMEOUT).unwrap();
        assert_eq!(
            inode,
            std::fs::metadata(dir.socket().with_extension("lock"))
                .unwrap()
                .ino()
        );
        drop(restarted);
    }

    #[test]
    fn shutdown_unlocks_descriptors_inherited_by_another_process() {
        let dir = TestDir::new();
        let (tx, _rx) = mpsc::channel();
        let server = listen_at(dir.socket(), tx.clone(), SERVER_TIMEOUT).unwrap();
        // dup and fork retain the same open file description. Keeping a dup
        // deterministically exercises the otherwise brief fork/exec window.
        let inherited = server._lock.0.try_clone().unwrap();
        drop(server);
        let restarted = listen_at(dir.socket(), tx, SERVER_TIMEOUT).unwrap();
        drop(restarted);
        drop(inherited);
    }

    #[test]
    fn acknowledgements_preserve_consecutive_command_order() {
        let dir = TestDir::new();
        let (tx, rx) = mpsc::channel();
        let _server = listen_at(dir.socket(), tx, SERVER_TIMEOUT).unwrap();
        for _ in 0..32 {
            send_to(&dir.socket(), Command::Start).unwrap();
            send_to(&dir.socket(), Command::Stop).unwrap();
            assert_eq!(rx.recv_timeout(CLIENT_TIMEOUT).unwrap(), Command::Start);
            assert_eq!(rx.recv_timeout(CLIENT_TIMEOUT).unwrap(), Command::Stop);
        }
    }

    #[test]
    fn stalled_and_invalid_clients_are_rejected_then_server_recovers() {
        let dir = TestDir::new();
        let (tx, rx) = mpsc::channel();
        let _server = listen_at(dir.socket(), tx, Duration::from_millis(100)).unwrap();
        let mut stalled = UnixStream::connect(dir.socket()).unwrap();
        send_to(&dir.socket(), Command::Start).unwrap();
        assert_eq!(rx.recv_timeout(CLIENT_TIMEOUT).unwrap(), Command::Start);
        assert_eq!(read_frame(&mut stalled, CLIENT_TIMEOUT).unwrap(), b"error");
        for bytes in [b"unknown\n".to_vec(), vec![b'x'; MAX_FRAME_BYTES + 1]] {
            let mut invalid = UnixStream::connect(dir.socket()).unwrap();
            invalid.write_all(&bytes).unwrap();
            assert_eq!(read_frame(&mut invalid, CLIENT_TIMEOUT).unwrap(), b"error");
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        }
        send_to(&dir.socket(), Command::Stop).unwrap();
        assert_eq!(rx.recv_timeout(CLIENT_TIMEOUT).unwrap(), Command::Stop);
    }

    #[test]
    fn a_closed_queue_never_acknowledges_success() {
        let dir = TestDir::new();
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let _server = listen_at(dir.socket(), tx, SERVER_TIMEOUT).unwrap();
        assert!(send_to(&dir.socket(), Command::Start).is_err());
    }

    #[test]
    fn saturated_backlog_fails_closed_without_replacing_the_socket() {
        let dir = TestDir::new();
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        listener
            .bind(&SockAddr::unix(dir.socket()).unwrap())
            .unwrap();
        listener.listen(0).unwrap();
        let _queued = connect_bounded(&dir.socket(), CLIENT_TIMEOUT).unwrap();
        assert_eq!(
            connect_bounded(&dir.socket(), CLIENT_TIMEOUT)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(send_to(&dir.socket(), Command::Start).is_err());
        let inode = std::fs::metadata(dir.socket()).unwrap().ino();
        let (tx, _rx) = mpsc::channel();
        assert!(listen_at(dir.socket(), tx, SERVER_TIMEOUT).is_err());
        assert_eq!(inode, std::fs::metadata(dir.socket()).unwrap().ino());
        drop(listener);
    }
}
