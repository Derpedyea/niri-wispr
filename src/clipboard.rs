use anyhow::{Context, Result, ensure};
use std::io::{Read, Seek, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const COPY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// GPUI's standard Wayland clipboard requires an input serial from a focused
/// window. Dictation deliberately never focuses its pill, so use data-control
/// via wl-copy. Its parent exits only after publication; its child serves the
/// selection until replaced, including after Dictation exits. Its exit status
/// alone is insufficient (wl-copy can exit 0 after a compositor disconnect),
/// so independently read back the exact bytes before authorizing delivery.
pub fn copy(text: &str, cancelled: &AtomicBool) -> Result<()> {
    let mut command = Command::new("wl-copy");
    command.args(["--type", "text/plain;charset=utf-8"]);
    let mut reader = Command::new("wl-paste");
    reader.args(["--no-newline", "--type", "text/plain;charset=utf-8"]);
    copy_using(text, cancelled, command, reader)
}

fn copy_using(
    text: &str,
    cancelled: &AtomicBool,
    publisher: Command,
    reader: Command,
) -> Result<()> {
    copy_with(text, cancelled, publisher, COPY_TIMEOUT)?;
    verify_with(text, cancelled, reader, COPY_TIMEOUT)
}

fn verify_with(
    text: &str,
    cancelled: &AtomicBool,
    mut reader: Command,
    timeout: Duration,
) -> Result<()> {
    let mut receipt = tempfile::tempfile().context("unable to prepare clipboard readback")?;
    reader.stdin(Stdio::null()).stdout(receipt.try_clone()?);
    run(reader, cancelled, timeout, Some(&receipt))?;
    receipt.rewind()?;
    let mut bytes = Vec::new();
    receipt
        .take(u64::try_from(MAX_TEXT_BYTES)? + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_TEXT_BYTES,
        "clipboard readback is too large"
    );
    ensure!(
        bytes == text.as_bytes(),
        "clipboard does not contain this transcript"
    );
    Ok(())
}

fn copy_with(
    text: &str,
    cancelled: &AtomicBool,
    mut command: Command,
    timeout: Duration,
) -> Result<()> {
    ensure!(
        text.len() <= MAX_TEXT_BYTES,
        "transcript is too large to copy"
    );
    ensure!(
        !cancelled.load(Ordering::Acquire),
        "clipboard copy cancelled"
    );
    // A seekable, anonymous file avoids an unbounded pipe write when the
    // compositor hangs before wl-copy starts reading stdin. Text is never an
    // argument (process-list exposure) or interpreted by a shell.
    let mut input = tempfile::tempfile().context("unable to prepare clipboard input")?;
    input.write_all(text.as_bytes())?;
    input.rewind()?;
    command.stdin(input).stdout(Stdio::null());
    run(command, cancelled, timeout, None)
}

fn run(
    mut command: Command,
    cancelled: &AtomicBool,
    timeout: Duration,
    output: Option<&std::fs::File>,
) -> Result<()> {
    ensure!(
        !cancelled.load(Ordering::Acquire),
        "clipboard copy cancelled"
    );
    let parent = libc::pid_t::try_from(std::process::id())?;
    // App exit can precede the worker's next cancellation check. Guard the
    // foreground publisher/reader too. A successful wl-copy's serving child
    // clears this signal on fork, so completed clipboard data still survives.
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
    let mut child = command
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .context("unable to start wl-copy; install wl-clipboard")?;
    let deadline = Instant::now() + timeout;
    let result = loop {
        if cancelled.load(Ordering::Acquire) {
            break Err(anyhow::anyhow!("clipboard copy cancelled"));
        }
        if Instant::now() >= deadline {
            break Err(anyhow::anyhow!("clipboard copy timed out"));
        }
        if let Some(output) = output {
            match output.metadata() {
                Ok(metadata) if metadata.len() <= u64::try_from(MAX_TEXT_BYTES)? => {}
                Ok(_) => break Err(anyhow::anyhow!("clipboard readback is too large")),
                Err(error) => break Err(error.into()),
            }
        }
        match exited(&child) {
            Ok(Some(true)) => {
                wait_owned(&mut child);
                return Ok(());
            }
            Ok(Some(false)) => break Err(anyhow::anyhow!("clipboard command failed")),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => break Err(error.into()),
        }
    };
    // Kill the whole group: a timed-out publisher must not subsequently fork
    // and replace a newer clipboard. Reap even when cancellation wins a race
    // with normal exit. ESRCH means the group has already exited.
    // Linux PIDs are positive pid_t values; keep that exact group ID until
    // reaping the leader, including on nonzero exit.
    let group = child.id() as libc::pid_t;
    loop {
        let killed = unsafe { libc::kill(-group, libc::SIGKILL) };
        if killed == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => break,
            Some(libc::EINTR) => continue,
            _ => {
                eprintln!("clipboard: kill failed, retrying: {error}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    wait_owned(&mut child);
    result
}

/// Inspect without reaping: retain the leader PID until failed publishers'
/// process groups are stopped, so cleanup cannot signal a reused PID/group.
fn exited(child: &Child) -> std::io::Result<Option<bool>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    Ok(Some(
        info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0,
    ))
}

fn wait_owned(child: &mut Child) {
    loop {
        match child.wait() {
            Ok(_) => return,
            Err(error) => {
                eprintln!("clipboard: wait failed, retrying: {error}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_exact_bytes_and_rejects_failed_publishers() {
        let text = "Café — exact text 👋\nSecond line.\n";
        let cancelled = AtomicBool::new(false);
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("copied");
        let mut command = Command::new("sh");
        command
            .args(["-c", "cat > \"$1\"", "copy-test"])
            .arg(&output);
        copy_with(text, &cancelled, command, COPY_TIMEOUT).unwrap();
        assert_eq!(std::fs::read(output).unwrap(), text.as_bytes());

        let mut command = Command::new("sh");
        command.args(["-c", "exit 1"]);
        assert!(copy_with(text, &cancelled, command, COPY_TIMEOUT).is_err());
        assert!(
            copy_with(
                text,
                &cancelled,
                Command::new("/missing/wl-copy"),
                COPY_TIMEOUT
            )
            .is_err()
        );
    }

    #[test]
    fn timeout_cancellation_and_size_limit_fail_closed() {
        let cancelled = AtomicBool::new(true);
        assert!(
            copy_with(
                "text",
                &cancelled,
                Command::new("/must/not/run"),
                COPY_TIMEOUT
            )
            .is_err()
        );
        cancelled.store(false, Ordering::Release);
        assert!(
            copy_with(
                &"x".repeat(MAX_TEXT_BYTES + 1),
                &cancelled,
                Command::new("/must/not/run"),
                COPY_TIMEOUT
            )
            .is_err()
        );
        let mut command = Command::new("sleep");
        command.arg("60");
        // Zero injected deadline deterministically takes the timeout path,
        // regardless of when the child is scheduled; no timing assertion.
        assert!(copy_with("text", &cancelled, command, Duration::ZERO).is_err());
    }

    #[test]
    fn cancellation_reaps_a_running_publisher() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("running");
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let child_cancel = cancelled.clone();
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo $$ > \"$1\"; exec sleep 60", "copy-test"])
            .arg(&marker);
        let worker =
            std::thread::spawn(move || copy_with("text", &child_cancel, command, COPY_TIMEOUT));
        // The fake I/O announces that the publisher is actually running.
        // Cancellation does not rely on racing a chosen sleep duration.
        let deadline = Instant::now() + COPY_TIMEOUT;
        while !marker.exists() || std::fs::read_to_string(&marker).unwrap().trim().is_empty() {
            assert!(Instant::now() < deadline, "publisher did not start");
            std::thread::yield_now();
        }
        let pid = std::fs::read_to_string(marker).unwrap();
        cancelled.store(true, Ordering::Release);
        assert!(worker.join().unwrap().is_err());
        assert!(!std::path::Path::new(&format!("/proc/{}", pid.trim())).exists());
    }

    #[test]
    fn publication_requires_exact_bounded_readback() {
        let cancelled = AtomicBool::new(false);
        let text = "Café — exact text 👋\n";
        let reader = |script: &str| {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        };
        copy_using(
            text,
            &cancelled,
            reader("exit 0"),
            reader("printf 'Café — exact text 👋\\n'"),
        )
        .unwrap();
        // A publisher's false-zero exit (real wl-copy on compositor loss)
        // cannot authorize insertion when readback fails or is stale.
        assert!(
            copy_using(
                text,
                &cancelled,
                reader("exit 0"),
                reader("printf 'previous copy'"),
            )
            .is_err()
        );
        assert!(copy_using(text, &cancelled, reader("exit 0"), reader("exit 1")).is_err());
        assert!(
            copy_using(
                text,
                &cancelled,
                reader("exit 0"),
                reader("head -c 16777217 /dev/zero"),
            )
            .is_err()
        );
    }

    #[test]
    fn parent_crash_stops_an_unfinished_publisher() {
        let directory = tempfile::tempdir().unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "clipboard::tests::parent_death_fixture",
                "--nocapture",
            ])
            .env("DICTATION_CLIPBOARD_TEST_PARENT", "supervisor")
            .env("DICTATION_CLIPBOARD_TEST_DIRECTORY", directory.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn parent_death_fixture() {
        let Some(role) = std::env::var_os("DICTATION_CLIPBOARD_TEST_PARENT") else {
            return;
        };
        let marker = std::path::PathBuf::from(
            std::env::var_os("DICTATION_CLIPBOARD_TEST_DIRECTORY").unwrap(),
        )
        .join("publisher.pid");
        if role == "parent" {
            let mut command = Command::new("sh");
            command
                .args(["-c", "echo $$ > \"$1\"; exec sleep 60", "copy-test"])
                .arg(marker);
            copy_with(
                "text",
                &AtomicBool::new(false),
                command,
                Duration::from_secs(60),
            )
            .unwrap();
            panic!("blocked publisher unexpectedly finished");
        }
        assert_eq!(role, "supervisor");
        // Only this subprocess adopts/reaps orphaned publishers; parallel
        // tests' child lifetimes stay independent.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "clipboard::tests::parent_death_fixture",
                "--nocapture",
            ])
            .env("DICTATION_CLIPBOARD_TEST_PARENT", "parent")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + COPY_TIMEOUT;
        while !marker.exists() || std::fs::read_to_string(&marker).unwrap().trim().is_empty() {
            if Instant::now() >= deadline {
                parent.kill().unwrap();
                wait_owned(&mut parent);
                panic!("publisher never started");
            }
            std::thread::yield_now();
        }
        let pid = std::fs::read_to_string(marker)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        parent.kill().unwrap();
        wait_owned(&mut parent);
        let mut status = 0;
        loop {
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if waited == pid {
                assert!(libc::WIFSIGNALED(status));
                assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
                return;
            }
            assert_eq!(waited, 0, "subreaper lost publisher");
            if Instant::now() >= deadline {
                assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
                assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
                panic!("publisher survived its parent crash");
            }
            std::thread::yield_now();
        }
    }
}
