// The daemon's side of the pipe: it sends one JSON view per line on stdin and
// reads one JSON event per line from stdout. Anything else must go to stderr.

// GPUIX logs to stdout; keep that out of the event stream.
console.log = console.error

export function send(event: Record<string, unknown>): void {
  process.stdout.write(JSON.stringify(event) + "\n")
}

/** Calls `onView` for each line; exits when the daemon closes the pipe. */
export async function listen<T>(onView: (view: T) => void): Promise<void> {
  const decoder = new TextDecoder()
  let buffered = ""
  for await (const chunk of process.stdin as AsyncIterable<Uint8Array | string>) {
    buffered += typeof chunk === "string" ? chunk : decoder.decode(chunk, { stream: true })
    let newline
    while ((newline = buffered.indexOf("\n")) >= 0) {
      const line = buffered.slice(0, newline)
      buffered = buffered.slice(newline + 1)
      if (line.trim()) onView(JSON.parse(line) as T)
    }
  }
  process.exit(0)
}

/** Shared with the daemon's views; GPUI does not inherit `color`. */
export const C = {
  bg: "#0e1117",
  card: "#161a22",
  cardBorder: "#232835",
  divider: "#1f2430",
  raised: "#262c3a",
  hover: "#1a1f29",
  inputBorder: "#2c3242",
  accent: "#8da2fb",
  text: "#e6e9ef",
  muted: "#8a92a3",
  faint: "#5b6375",
  red: "#f87171",
}
