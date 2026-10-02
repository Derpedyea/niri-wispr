use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_TYPING_TIME: Duration = Duration::from_secs(300);

/// wtype supplies a per-transcript Unicode keymap through Wayland's virtual
/// keyboard protocol. Physical layout/locks cannot reinterpret its characters,
/// and these events never appear on the evdev hotkey listener.
pub struct Typer {
    executable: PathBuf,
}

impl Typer {
    #[cfg(test)]
    pub fn for_test(executable: PathBuf) -> Self {
        Self { executable }
    }

    pub fn new() -> Result<Typer> {
        let search =
            std::env::var_os("PATH").context("PATH is unset; install wtype to enable typing")?;
        for directory in std::env::split_paths(&search) {
            let executable = directory.join("wtype");
            match executable.metadata() {
                Ok(metadata)
                    if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 =>
                {
                    return Ok(Typer { executable });
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("unable to check wtype executable"),
            }
        }
        bail!("wtype is missing; install it to enable Wayland Unicode typing")
    }

    /// Cancellation also owns the child lifetime. Anonymous files avoid stdin
    /// pipe stalls and leave no transcript/keymap files after cancellation or a
    /// crash. Errors retain the application's clipboard fallback.
    pub fn type_str(&mut self, text: &str, cancelled: &AtomicBool) -> Result<()> {
        validate_text(text)?;
        if cancelled.load(Ordering::SeqCst) {
            bail!("typing cancelled");
        }
        if text.is_empty() {
            return Ok(());
        }
        let mut input = tempfile::tempfile().context("unable to create anonymous typing input")?;
        input
            .write_all(text.as_bytes())
            .context("unable to write typing input")?;
        input.rewind().context("unable to rewind typing input")?;
        let mut errors =
            tempfile::tempfile().context("unable to create anonymous typing diagnostics")?;
        let diagnostics = errors
            .try_clone()
            .context("unable to clone typing diagnostics")?;
        if cancelled.load(Ordering::SeqCst) {
            bail!("typing cancelled");
        }
        let mut command = Command::new(&self.executable);
        command
            .arg("-")
            .env("LC_ALL", "C.UTF-8")
            .stdin(Stdio::from(input))
            .stdout(Stdio::null())
            .stderr(Stdio::from(diagnostics));
        let parent =
            libc::pid_t::try_from(std::process::id()).context("parent PID out of range")?;
        // Only async-signal-safe system calls run between fork and exec. The
        // parent check covers its death before PR_SET_PDEATHSIG is installed.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
        let child = command.spawn().context("unable to start wtype")?;
        let mut child = RunningChild::new(child);
        let started = Instant::now();
        let timeout = typing_timeout(text)?;
        let outcome = wait_for_typing(
            child.process(),
            cancelled,
            timeout,
            || started.elapsed(),
            thread::sleep,
        );
        // A cancelled/failed/timed-out process is killed and reaped before the
        // worker returns. Cleanup errors are retried; they never orphan input.
        child.finish();
        match outcome {
            Ok(()) => Ok(()),
            Err(error) => {
                let details = read_diagnostics(&mut errors).with_context(|| {
                    format!("typing failed: {error:#}; diagnostics unavailable")
                })?;
                if details.is_empty() {
                    Err(error)
                } else {
                    Err(error.context(format!("wtype: {details}")))
                }
            }
        }
    }
}

fn validate_text(text: &str) -> Result<()> {
    if let Some(control) = text
        .chars()
        .find(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        bail!(
            "transcript contains unsupported control character U+{:04X}",
            u32::from(control)
        );
    }
    Ok(())
}

fn typing_timeout(text: &str) -> Result<Duration> {
    let characters =
        u64::try_from(text.chars().count()).context("transcript is too large to type")?;
    Ok(
        Duration::from_millis(characters.saturating_mul(20).saturating_add(5_000))
            .min(MAX_TYPING_TIME),
    )
}

fn read_diagnostics(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))
        .context("unable to rewind typing diagnostics")?;
    let mut bytes = Vec::new();
    file.take(4096)
        .read_to_end(&mut bytes)
        .context("unable to read typing diagnostics")?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

trait TypingProcess {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> std::io::Result<ExitStatus>;
}

impl TypingProcess for Child {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }
    fn kill(&mut self) -> std::io::Result<()> {
        Child::kill(self)
    }
    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        Child::wait(self)
    }
}

fn wait_for_typing(
    process: &mut impl TypingProcess,
    cancelled: &AtomicBool,
    timeout: Duration,
    mut elapsed: impl FnMut() -> Duration,
    mut pause: impl FnMut(Duration),
) -> Result<()> {
    loop {
        if cancelled.load(Ordering::SeqCst) {
            bail!("typing cancelled");
        }
        match process.try_wait().context("unable to check wtype status")? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => bail!("wtype exited with {status}"),
            None => {}
        }
        if elapsed() >= timeout {
            bail!("typing timed out");
        }
        pause(POLL_INTERVAL);
    }
}

fn reap_process(process: &mut impl TypingProcess, mut pause: impl FnMut(Duration)) {
    loop {
        match process.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => eprintln!("typing status during cleanup failed: {error}; retrying"),
        }
        if let Err(error) = process.kill() {
            eprintln!("typing process termination failed: {error}; retrying cleanup");
            pause(POLL_INTERVAL);
            continue;
        }
        match process.wait() {
            Ok(_) => return,
            Err(error) => {
                eprintln!("typing process reap failed: {error}; retrying cleanup");
                pause(POLL_INTERVAL);
            }
        }
    }
}

struct RunningChild {
    child: Option<Child>,
}

impl RunningChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }
    fn process(&mut self) -> &mut Child {
        self.child.as_mut().expect("typing child is still owned")
    }
    fn finish(&mut self) {
        if let Some(mut child) = self.child.take() {
            reap_process(&mut child, thread::sleep);
        }
    }
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Explicit opt-in hardware test seam; production never creates a uinput device.
#[cfg(test)]
pub struct VirtualKeyboard {
    dev: evdev::uinput::VirtualDevice,
}

#[cfg(test)]
impl VirtualKeyboard {
    pub fn new() -> Result<Self> {
        let mut keys = evdev::AttributeSet::new();
        for key in [
            evdev::KeyCode::KEY_A,
            evdev::KeyCode::KEY_Z,
            evdev::KeyCode::KEY_F24,
        ] {
            keys.insert(key);
        }
        let dev = evdev::uinput::VirtualDevice::builder()?
            .name("dictationapp test keyboard")
            .with_keys(&keys)?
            .build()?;
        Ok(Self { dev })
    }
    pub fn emit(&mut self, key: evdev::KeyCode, value: i32) -> Result<()> {
        self.dev.emit(&[*evdev::KeyEvent::new(key, value)])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;

    struct FakeProcess {
        statuses: VecDeque<std::io::Result<Option<ExitStatus>>>,
        kill_failures: usize,
        reap_failures: usize,
        killed: usize,
        reaped: usize,
    }
    impl FakeProcess {
        fn pending() -> Self {
            Self {
                statuses: VecDeque::new(),
                kill_failures: 0,
                reap_failures: 0,
                killed: 0,
                reaped: 0,
            }
        }
    }
    impl TypingProcess for FakeProcess {
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            self.statuses.pop_front().unwrap_or(Ok(None))
        }
        fn kill(&mut self) -> std::io::Result<()> {
            self.killed += 1;
            if self.kill_failures > 0 {
                self.kill_failures -= 1;
                return Err(std::io::Error::other("injected kill failure"));
            }
            Ok(())
        }
        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            self.reaped += 1;
            if self.reap_failures > 0 {
                self.reap_failures -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            Ok(ExitStatus::from_raw(0))
        }
    }

    #[test]
    fn unicode_input_remains_exact_and_controls_fail_closed() {
        let text = "Café — Привет こんにちは 👩🏽‍💻\n\t-- -M ctrl";
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-wtype");
        let captured = directory.path().join("captured");
        let quoted = format!(
            "'{}'",
            captured.display().to_string().replace('\'', "'\"'\"'")
        );
        std::fs::write(&executable, format!("#!/bin/sh\n[ \"$1\" = - ] || exit 3\n[ \"$LC_ALL\" = C.UTF-8 ] || exit 4\ncat > {quoted}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cancelled = AtomicBool::new(false);
        let mut typer = Typer::for_test(executable);
        typer.type_str(text, &cancelled).unwrap();
        assert_eq!(std::fs::read_to_string(&captured).unwrap(), text);
        assert!(typer.type_str("before\0after", &cancelled).is_err());
        assert!(typer.type_str("before\x1bafter", &cancelled).is_err());
        assert_eq!(std::fs::read_to_string(&captured).unwrap(), text);
    }

    #[test]
    fn cancellation_timeout_and_status_failures_stop_typing() {
        let cancelled = AtomicBool::new(false);
        let elapsed = Cell::new(Duration::ZERO);
        let mut process = FakeProcess::pending();
        let result = wait_for_typing(
            &mut process,
            &cancelled,
            Duration::from_secs(1),
            || elapsed.get(),
            |duration| {
                elapsed.set(elapsed.get() + duration);
                cancelled.store(true, Ordering::SeqCst);
            },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        cancelled.store(false, Ordering::SeqCst);
        let result = wait_for_typing(
            &mut process,
            &cancelled,
            Duration::from_millis(30),
            || elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
        process
            .statuses
            .push_back(Err(std::io::Error::other("injected status failure")));
        assert!(
            wait_for_typing(
                &mut process,
                &cancelled,
                Duration::from_secs(1),
                || Duration::ZERO,
                |_| panic!("status failure must fail immediately")
            )
            .is_err()
        );
    }

    #[test]
    fn successful_and_failed_exits_are_distinct() {
        let cancelled = AtomicBool::new(false);
        for (status, success) in [(0, true), (256, false)] {
            let mut process = FakeProcess::pending();
            process
                .statuses
                .push_back(Ok(Some(ExitStatus::from_raw(status))));
            assert_eq!(
                wait_for_typing(
                    &mut process,
                    &cancelled,
                    Duration::from_secs(1),
                    || Duration::ZERO,
                    |_| panic!("finished child must not poll again")
                )
                .is_ok(),
                success
            );
        }
    }

    #[test]
    fn failed_cleanup_is_retried_until_child_is_reaped() {
        let mut process = FakeProcess::pending();
        process.kill_failures = 1;
        process.reap_failures = 1;
        let retries = Cell::new(0);
        reap_process(&mut process, |_| retries.set(retries.get() + 1));
        assert_eq!(process.killed, 3);
        assert_eq!(process.reaped, 2);
        assert_eq!(retries.get(), 2);
    }
}
