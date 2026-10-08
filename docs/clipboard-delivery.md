# Clipboard delivery

Invariant: a transcript is eligible for typing or a copied notice only after an
independent clipboard read returns its exact bytes, without focusing Dictation.

| Exit | Behavior → test |
| --- | --- |
| Done | Exact UTF-8 text is published before typing; clipboard-only completion shows “Copied. Paste to insert.” → isolated niri reader + app delivery tests |
| Failed | Publisher/reader failure, timeout, stale or oversized readback stops delivery and shows “Copy failed. Check wl-clipboard.”; no copied notice or typing → false-success/mismatch/reader-failure tests |
| Cancelled | `--cancel` continues to discard recording and pending starts only; shutdown cancels an in-flight publisher → process cancellation test |
| Retried | A new dictation runs a new publisher, so an earlier failure cannot authorize it → failed then successful app test |
| Duplicated | Busy starts coalesce; every copy owns a fresh process/input file → pending-start tests + repeated native delivery |
| Reordered | Commands are drained before copying and before choosing typing; an ended pending gesture does not restart → queued-command tests |
| Crash mid-step | Parent-death signal kills an unfinished publisher/reader. A publisher already serving the clipboard may outlive Dictation → isolated parent-death regression + native publisher lifetime check |
| Unmount | Dropping the view cancels the publisher/typer; a completed clipboard remains owned by wl-copy until replaced → cancellation + native check |
| Sign-out | No account/session state is introduced; API-key handling is unchanged → out of scope |
| Identity change | No identity-dependent storage or shared transcript history is introduced → out of scope |

The background publisher owns an anonymous input file and a Wayland clipboard
source. After successful publication, wl-copy's child serves repeated paste
requests until another application replaces the clipboard. Dictation does not
restore an old clipboard or republish after replacement. Browser clipboard
managers can independently retain copies; this change neither enables nor
configures them.

## Product references

- [Wispr Flow](https://docs.wisprflow.ai/articles/6409258247-starting-your-first-dictation)
  exposes clipboard recovery when insertion fails. Borrow an actionable paste
  message, without adding history or a recovery panel.
- [Superwhisper](https://superwhisper.com/docs/get-started/settings-advanced)
  keeps results on the clipboard when automatic paste is off. Borrow explicit
  manual-paste completion.
- [Handy](https://handy.computer/docs/advanced.html) documents Linux overlay focus
  problems. Preserve the target's focus and remove the native pill after its
  message expires.

## Provider contract and footprint

Use `wl-copy --type text/plain;charset=utf-8` with an input file on stdin, then
`wl-paste --no-newline --type text/plain;charset=utf-8` to verify its exact bytes. No
shell interpolation, transcript command-line arguments, MIME inference, primary
selection, paste-once, watch mode, or new service. `wl-clipboard` becomes a runtime
dependency in both AUR packages. It uses the current Wayland session and an
anonymous temporary input file; wl-copy also creates and unlinks its own input
spool before serving requests. It adds no network endpoint.

[wl-clipboard's source](https://github.com/bugaevc/wl-clipboard/blob/master/src/wl-copy.c)
documents its completion callback: “We fork our process and leave the child
running in the background, while exiting in the parent.” Its copy action invokes
that callback after setting the selection. Its unchecked roundtrip in
[copy-action.c](https://github.com/bugaevc/wl-clipboard/blob/master/src/types/copy-action.c)
can report zero after a compositor disconnect; an isolated socket reproduction
confirmed this in wl-copy 2.3.0. Exit status alone never authorizes delivery.
The foreground publisher and verifier are bounded and cancelled together with
their process group on failure/shutdown. Failed cleanup is retried; waitid with
WNOWAIT retains the group leader PID until cleanup, avoiding reused-PID signals.

[Linux's parent-death signal](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html)
protects both subprocesses if the app exits before its worker polls cancellation.
The setting is cleared in forked children, so a successfully detached clipboard
owner survives normally. The getppid check closes the guard-installation race.

[The manual](https://man.archlinux.org/man/wl-copy.1.en) describes a popup fallback
on compositors without data-control. niri supplies data-control; native checks
must prove publication keeps the target focused and creates no helper window.

## Known gaps and deferred scope

- A clipboard publisher failure requires correcting wl-clipboard/the session
  and dictating again; no transcript history or retry store is added.
- Typing can fail after a prefix was inserted. “Typing failed. Full text copied.”
  signals recovery; inspect/replace partial insertion before pasting the full
  transcript. This change adds no automatic rollback.
- Clipboard readback is a snapshot. A subsequent user/manager copy can replace
  it normally; Dictation never restores or republishes an older transcript.
- Compositors without data-control can use wl-copy's documented popup fallback;
  focus preservation is verified on niri only.
- Cancelling transcription/cleanup/typing, clipboard restoration, setup errors
  with clipped text, onboarding, and transcript history are separate work.
- Headless delivery checks inject a fixed transcript at the completed API
  boundary. They exercise real publication, paste/typing, focus, and pill lifetime;
  live speech recognition remains an opt-in network/hardware check.

## Native verification

```sh
cargo test --locked --no-run --message-format=json # find the test executable
cargo build --locked
python3 scripts/check-delivery.py <test-executable> target/debug/dictationapp \
  --evidence .t3/pr-evidence
```

The check uses a separate niri, browser profile, IPC socket and config, with no
hotkey watcher or real microphone/API key. It checks external clipboard reads,
a native Ctrl+V in Chrome, typing, both delivery failures, focus, pill expiry,
and preservation of a newer clipboard through shutdown. It hashes
`/proc/<pid>/exe` against the test executable before trusting any result.
