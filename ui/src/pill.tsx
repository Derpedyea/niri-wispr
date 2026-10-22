// The recording pill: a bottom-anchored layer-shell overlay that never takes
// keyboard focus, so typed text lands in the window the user was in.

import { useEffect, useState } from "react"
import { render } from "@gpuix/react"
import { listen, send } from "./link"

const BARS = 21

export interface PillView {
  dot: string
  kind: "recording" | "busy" | "message"
  /** Recent input levels, 0–1, oldest first (recording only). */
  bars: number[]
  label: string
  message: string
}

let latest: PillView | null = null
let update: ((view: PillView) => void) | null = null

function useBusyPhase(active: boolean): number {
  const [phase, setPhase] = useState(0)
  useEffect(() => {
    if (!active) return
    const start = performance.now()
    const timer = setInterval(() => setPhase((performance.now() - start) / 1000), 33)
    return () => clearInterval(timer)
  }, [active])
  return phase
}

function Pill({ initial }: { initial: PillView }) {
  const [view, setView] = useState(initial)
  useEffect(() => {
    update = setView
    if (latest) setView(latest)
  }, [])
  const phase = useBusyPhase(view.kind === "busy")
  const heights = Array.from({ length: BARS }, (_, i) =>
    view.kind === "recording"
      ? 3 + (view.bars[i] ?? 0) * 25
      : 3 + Math.pow(Math.sin(phase * 6 - i * 0.55) * 0.5 + 0.5, 4) * 13,
  )
  return (
    <div style={{ width: "100%", height: "100%", padding: 4, display: "flex", alignItems: "center" }}>
      <div
        onClick={() => send({ event: "click" })}
        style={{
          width: "100%", height: 56, borderRadius: 9999, backgroundColor: "#11141bf2",
          borderWidth: 1, borderColor: "#ffffff22", paddingLeft: 16, paddingRight: 16,
          display: "flex", flexDirection: "row", alignItems: "center", gap: 12, cursor: "pointer",
        }}
      >
        <div style={{ width: 8, height: 8, borderRadius: 9999, backgroundColor: view.dot, flexShrink: 0 }} />
        {view.kind === "message" ? (
          <text style={{ flexGrow: 1, minWidth: 0, fontSize: 14, color: "#f2f4f8", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
            {view.message}
          </text>
        ) : (
          <>
            <div style={{ flexGrow: 1, height: 32, display: "flex", flexDirection: "row", alignItems: "center", justifyContent: "center", gap: 2 }}>
              {heights.map((h, i) => (
                <div key={i} style={{ width: 2, height: h, borderRadius: 9999, backgroundColor: view.kind === "recording" ? "#f1f4fa" : "#8da2fb" }} />
              ))}
            </div>
            <text style={{ fontSize: 14, color: "#aeb6c5", whiteSpace: "nowrap" }}>{view.label}</text>
          </>
        )}
      </div>
    </div>
  )
}

/** Started warm when a recording begins; the window opens with the first view. */
export function runPill(): void {
  void listen<PillView>((view) => {
    const opened = latest != null
    latest = view
    if (update) return update(view)
    if (opened) return
    render(<Pill initial={view} />, {
      title: "Dictation",
      appId: "dictationapp",
      width: 320,
      height: 64,
      transparent: true,
      focus: false,
      resizable: false,
      layerShell: {
        namespace: "dictationapp-pill",
        layer: "overlay",
        anchor: ["bottom"],
        margin: [0, 0, 80, 0],
        keyboardInteractivity: "none",
      },
    })
  })
}
