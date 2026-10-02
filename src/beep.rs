//! Subtle audio cues for recording start/stop. Playback workers own and reap
//! their children; shutdown cancels and joins them before the daemon exits.

use anyhow::{Context, Result};
use std::f32::consts::TAU;
use std::io::Cursor;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const PLAYBACK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy)]
enum Cue {
    Start,
    Stop,
    Error,
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("dictationapp")
}

fn cue_path(cue: Cue) -> PathBuf {
    cache_dir().join(match cue {
        Cue::Start => "start.wav",
        Cue::Stop => "stop.wav",
        Cue::Error => "error.wav",
    })
}

fn segments(cue: Cue) -> &'static [(f32, u32)] {
    match cue {
        Cue::Start => &[(660.0, 60), (990.0, 90)],
        Cue::Stop => &[(880.0, 60), (550.0, 90)],
        Cue::Error => &[(220.0, 180)],
    }
}

struct TemporaryWav(PathBuf);

impl Drop for TemporaryWav {
    fn drop(&mut self) {
        if let Err(error) = remove_temporary(&self.0) {
            eprintln!("beep: temporary WAV cleanup failed: {error:#}");
        }
    }
}

fn remove_temporary(path: &Path) -> Result<()> {
    for attempt in 0..3 {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if attempt == 2 => {
                return Err(error).context("could not remove temporary WAV");
            }
            Err(_) => thread::yield_now(),
        }
    }
    unreachable!()
}

fn ensure_wav(cue: Cue) -> Result<PathBuf> {
    let path = cue_path(cue);
    let temporary = TemporaryWav(path.with_extension("wav.tmp"));
    // A crash can leave this temporary file, but never a partial published WAV.
    // Calls are serialized by the playback registry; retry stale cleanup first.
    remove_temporary(&temporary.0)?;
    let expected_samples: usize = segments(cue)
        .iter()
        .map(|(_, ms)| (RATE as u64 * u64::from(*ms) / 1000) as usize)
        .sum();
    if let Ok(mut reader) = hound::WavReader::open(&path)
        && reader.spec().channels == 1
        && reader.spec().sample_rate == RATE
        && reader.spec().bits_per_sample == 16
        && reader.duration() as usize == expected_samples
        && reader.samples::<i16>().all(|sample| sample.is_ok())
    {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().context("cue path has no parent")?)?;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut encoded = Cursor::new(Vec::new());
    let mut writer = hound::WavWriter::new(&mut encoded, spec)?;
    for &(freq, ms) in segments(cue) {
        let n = (RATE as u64 * ms as u64 / 1000) as u32;
        for i in 0..n {
            // Raised-cosine envelope on the outer 12ms avoids clicks.
            let fade = (RATE as f32 * 0.012).min(n as f32 / 2.0);
            let amp = if i as f32 > n as f32 - fade {
                (n as f32 - i as f32) / fade
            } else {
                (i as f32 / fade).min(1.0)
            };
            let sample = (i as f32 * freq * TAU / RATE as f32).sin() * amp * 0.25;
            writer.write_sample((sample * i16::MAX as f32) as i16)?;
        }
    }
    writer.finalize()?;
    std::fs::write(&temporary.0, encoded.into_inner())?;
    std::fs::rename(&temporary.0, &path)?;
    Ok(path)
}

#[derive(Default)]
struct PlaybackWorkers {
    stopped: Arc<AtomicBool>,
    workers: Vec<JoinHandle<Result<()>>>,
}

static PLAYBACK: OnceLock<Mutex<PlaybackWorkers>> = OnceLock::new();

fn report_worker(worker: JoinHandle<Result<()>>) {
    match worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("beep: {error:#}"),
        Err(_) => eprintln!("beep: playback worker panicked"),
    }
}

/// Cancel current cues and reap their players before normal application exit.
/// After a crash the OS reparents outstanding children to its process reaper.
pub fn shutdown() {
    let Some(playback) = PLAYBACK.get() else {
        return;
    };
    let Ok(mut playback) = playback.lock() else {
        eprintln!("beep: playback registry is poisoned");
        return;
    };
    playback.stopped.store(true, Ordering::Release);
    for worker in playback.workers.drain(..) {
        worker.thread().unpark();
        report_worker(worker);
    }
}

fn wait_owned(child: &mut Child) -> ExitStatus {
    loop {
        match child.wait() {
            Ok(status) => return status,
            Err(error) => {
                // Never drop an unreaped child because a cleanup operation failed.
                eprintln!("beep: player wait failed, retrying: {error}");
                if let Err(error) = child.kill() {
                    eprintln!("beep: player kill failed, retrying wait: {error}");
                }
                thread::park_timeout(Duration::from_millis(10));
            }
        }
    }
}

fn stop_owned(child: &mut Child) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => eprintln!("beep: cleanup status failed, retrying: {error}"),
        }
        match child.kill() {
            Ok(()) => {
                wait_owned(child);
                return;
            }
            Err(error) => {
                // Failed termination does not justify blocking on a live child.
                eprintln!("beep: player kill failed, retrying: {error}");
                thread::park_timeout(Duration::from_millis(10));
            }
        }
    }
}

fn play_file_with(
    path: &Path,
    players: &[(&Path, &[&str])],
    stopped: &AtomicBool,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    play_file_with_clock(
        path,
        players,
        stopped,
        timeout,
        || started.elapsed(),
        || thread::park_timeout(Duration::from_millis(10)),
    )
}

fn play_file_with_clock(
    path: &Path,
    players: &[(&Path, &[&str])],
    stopped: &AtomicBool,
    timeout: Duration,
    now: impl Fn() -> Duration,
    wait: impl Fn(),
) -> Result<()> {
    let mut failures = Vec::new();
    for (bin, args) in players {
        if stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let started = now();
        let mut command = Command::new(bin);
        command.args(*args).arg(path);
        let parent =
            libc::pid_t::try_from(std::process::id()).context("parent PID out of range")?;
        // The player must also stop after a crash. Only async-signal-safe
        // calls run after fork; getppid closes the guard-installation race.
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
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                failures.push(format!("{}: {error}", bin.display()));
                continue;
            }
        };
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    failures.push(format!("{} exited with {status}", bin.display()));
                    break;
                }
                Err(error) => {
                    stop_owned(&mut child);
                    failures.push(format!("{}: {error}", bin.display()));
                    break;
                }
                Ok(None) => {}
            }
            if stopped.load(Ordering::Acquire) {
                stop_owned(&mut child);
                return Ok(());
            }
            if now().saturating_sub(started) >= timeout {
                stop_owned(&mut child);
                failures.push(format!("{} timed out", bin.display()));
                break;
            }
            wait();
        }
    }
    anyhow::bail!("no audio player succeeded: {}", failures.join("; "))
}

pub fn start() {
    play(Cue::Start);
}
pub fn stop() {
    play(Cue::Stop);
}
pub fn error() {
    play(Cue::Error);
}

fn play(cue: Cue) {
    let result = (|| -> Result<()> {
        let mut playback = PLAYBACK
            .get_or_init(|| Mutex::new(PlaybackWorkers::default()))
            .lock()
            .map_err(|_| anyhow::anyhow!("playback registry is poisoned"))?;
        if playback.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut index = 0;
        while index < playback.workers.len() {
            if playback.workers[index].is_finished() {
                report_worker(playback.workers.swap_remove(index));
            } else {
                index += 1;
            }
        }
        let path = ensure_wav(cue)?;
        let stopped = playback.stopped.clone();
        let worker = thread::Builder::new()
            .name("dictation-beep".into())
            .spawn(move || {
                play_file_with(
                    &path,
                    &[
                        (Path::new("pw-play"), &[]),
                        (Path::new("paplay"), &[]),
                        (Path::new("aplay"), &["-q"]),
                    ],
                    &stopped,
                    PLAYBACK_TIMEOUT,
                )
            })
            .context("could not start playback worker")?;
        playback.workers.push(worker);
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("beep: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    use std::sync::atomic::AtomicUsize;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = PathBuf::from(format!(
                "/tmp/dictation-beep-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn player(&self, name: &str, action: &str) -> PathBuf {
            let player = self.0.join(name);
            let pid = self.0.join(format!("{name}.pid"));
            std::fs::write(
                &player,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\n{action}\n",
                    pid.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o755)).unwrap();
            player
        }

        fn assert_reaped(&self, name: &str) {
            let pid = std::fs::read_to_string(self.0.join(format!("{name}.pid"))).unwrap();
            assert!(!PathBuf::from(format!("/proc/{}", pid.trim())).exists());
        }

        fn player_ready(path: &Path) -> bool {
            match std::fs::read_to_string(path) {
                Ok(pid) => pid.ends_with('\n') && pid.trim().parse::<u32>().is_ok(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => panic!("could not read fake player readiness: {error}"),
            }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn missing_and_failed_players_fall_back_and_are_reaped() {
        let dir = TestDir::new();
        let failed = dir.player("failed", "exit 23");
        let successful = dir.player("successful", "exit 0");
        let failed_script = failed.to_str().unwrap();
        let successful_script = successful.to_str().unwrap();
        play_file_with_clock(
            Path::new("unused.wav"),
            &[
                (&dir.0.join("missing"), &[]),
                (Path::new("/bin/sh"), &[failed_script]),
                (Path::new("/bin/sh"), &[successful_script]),
            ],
            &AtomicBool::new(false),
            PLAYBACK_TIMEOUT,
            || Duration::ZERO,
            thread::yield_now,
        )
        .unwrap();
        dir.assert_reaped("failed");
        dir.assert_reaped("successful");
    }

    #[test]
    fn cancellation_kills_and_reaps_an_active_player() {
        let dir = TestDir::new();
        let fifo = dir.0.join("blocked-input");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let blocked = dir.player("blocked", &format!("read ignored < '{}'", fifo.display()));
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_worker = stopped.clone();
        let worker = thread::spawn(move || {
            play_file_with_clock(
                Path::new("unused.wav"),
                &[(Path::new("/bin/sh"), &[blocked.to_str().unwrap()])],
                &stopped_worker,
                PLAYBACK_TIMEOUT,
                || Duration::ZERO,
                thread::yield_now,
            )
        });
        let deadline = Instant::now() + CLIENT_TEST_TIMEOUT;
        loop {
            if TestDir::player_ready(&dir.0.join("blocked.pid")) {
                break;
            }
            assert!(Instant::now() < deadline, "fake player never started");
            thread::yield_now();
        }
        stopped.store(true, Ordering::Release);
        worker.thread().unpark();
        worker.join().unwrap().unwrap();
        dir.assert_reaped("blocked");
    }

    const CLIENT_TEST_TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn timeout_kills_and_reaps_a_stuck_player() {
        let dir = TestDir::new();
        let fifo = dir.0.join("blocked-input");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let blocked = dir.player("blocked", &format!("read ignored < '{}'", fifo.display()));
        let pid = dir.0.join("blocked.pid");
        let watchdog = Instant::now() + CLIENT_TEST_TIMEOUT;
        assert!(
            play_file_with_clock(
                Path::new("unused.wav"),
                &[(Path::new("/bin/sh"), &[blocked.to_str().unwrap()])],
                &AtomicBool::new(false),
                Duration::from_millis(100),
                || {
                    if TestDir::player_ready(&pid) {
                        Duration::from_secs(1)
                    } else {
                        Duration::ZERO
                    }
                },
                || {
                    assert!(Instant::now() < watchdog, "fake player never started");
                    thread::yield_now();
                },
            )
            .is_err()
        );
        dir.assert_reaped("blocked");
    }

    struct TestParent(Child);

    impl Drop for TestParent {
        fn drop(&mut self) {
            stop_owned(&mut self.0);
        }
    }

    #[test]
    fn parent_crash_stops_an_active_player() {
        let dir = TestDir::new();
        let fifo = dir.0.join("blocked-input");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        dir.player("blocked", &format!("read ignored < '{}'", fifo.display()));
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "beep::tests::parent_death_fixture",
                "--nocapture",
            ])
            .env("DICTATION_BEEP_TEST_FIXTURE", "supervisor")
            .env("DICTATION_BEEP_TEST_DIRECTORY", &dir.0)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    // Re-executed only by the crash regression. The subreaper is confined to
    // this subprocess, so concurrent tests keep their own child lifetimes.
    #[test]
    fn parent_death_fixture() {
        let Some(role) = std::env::var_os("DICTATION_BEEP_TEST_FIXTURE") else {
            return;
        };
        let directory = PathBuf::from(std::env::var_os("DICTATION_BEEP_TEST_DIRECTORY").unwrap());
        let player = directory.join("blocked");
        if role == "parent" {
            play_file_with(
                Path::new("unused.wav"),
                &[(Path::new("/bin/sh"), &[player.to_str().unwrap()])],
                &AtomicBool::new(false),
                Duration::from_secs(3600),
            )
            .unwrap();
            panic!("blocked fake player unexpectedly finished");
        }
        assert_eq!(role, "supervisor");
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let mut parent = TestParent(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "beep::tests::parent_death_fixture",
                    "--nocapture",
                ])
                .env("DICTATION_BEEP_TEST_FIXTURE", "parent")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let marker = directory.join("blocked.pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !TestDir::player_ready(&marker) {
            assert!(
                Instant::now() < deadline,
                "crash fixture player never started"
            );
            thread::yield_now();
        }
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        parent.0.kill().unwrap();
        parent.0.wait().unwrap();
        let mut status = 0;
        loop {
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if waited == pid {
                assert!(libc::WIFSIGNALED(status));
                assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
                return;
            }
            assert_eq!(
                waited,
                0,
                "subreaper lost player: {}",
                std::io::Error::last_os_error()
            );
            if Instant::now() >= deadline {
                // Reap the injected failure too, so a failed regression never
                // leaves its intentionally stuck process behind.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                panic!("player survived its parent crash");
            }
            thread::yield_now();
        }
    }
}
