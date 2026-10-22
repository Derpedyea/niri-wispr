// Settings: a view of the daemon's settings model. Every change is sent as an
// event; the daemon saves config.toml, reloads, and answers with a new view.

import { useEffect, useRef, useState, type ReactNode } from "react"
import { render, useGpuix } from "@gpuix/react"
import { C, listen, send } from "./link"

export interface SettingsView {
  hint: string
  keyLabel: string
  capturing: boolean
  keyNote: string | null
  keyNoteError: boolean
  mode: "hold" | "toggle"
  mics: { value: string | null; label: string; detail: string; selected: boolean }[]
  envKey: boolean
  apiKey: string
  models: { label: string; slug: string }[]
  model: string
  customModel: boolean
  language: string
  cleanup: boolean
  typeText: boolean
  beeps: boolean
  banner: string | null
}

type Field = "apiKey" | "model" | "language"

let latest: SettingsView | null = null
let update: ((view: SettingsView) => void) | null = null
let editing: Field | null = null

function Section({ title, children }: { title: string; children: ReactNode[] }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
      <text style={{ paddingLeft: 4, fontSize: 11.5, fontWeight: 600, color: C.muted }}>{title}</text>
      <div style={{ display: "flex", flexDirection: "column", borderRadius: 12, backgroundColor: C.card, borderWidth: 1, borderColor: C.cardBorder, overflow: "hidden" }}>
        {children.filter(Boolean).map((child, i) => (
          <div key={i} style={i > 0 ? { borderTopWidth: 1, borderColor: C.divider } : {}}>{child}</div>
        ))}
      </div>
    </div>
  )
}

function Label({ label, detail, detailColor = C.muted, children }: { label: string; detail?: string | null; detailColor?: string; children?: ReactNode }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", minWidth: 0, gap: 2, flexShrink: 1 }}>
      <text style={{ fontSize: 13.5, color: C.text, whiteSpace: "nowrap" }}>{label}</text>
      {detail ? <text style={{ fontSize: 12, lineHeight: 16, color: detailColor }}>{detail}</text> : null}
      {children}
    </div>
  )
}

function Row({ label, control, onClick }: { label: ReactNode; control: ReactNode; onClick?: () => void }) {
  return (
    <div
      onClick={onClick}
      style={{
        display: "flex", flexDirection: "row", alignItems: "center", justifyContent: "space-between", gap: 12,
        minHeight: 40, paddingLeft: 14, paddingRight: 14, paddingTop: 6, paddingBottom: 6,
        ...(onClick ? { cursor: "pointer", hover: { backgroundColor: C.hover } } : {}),
      }}
    >
      {label}
      <div style={{ flexShrink: 0 }}>{control}</div>
    </div>
  )
}

// Switch and Radio only draw state: clicks go to the row around them, which
// a styled child div would otherwise swallow. GPUIX doesn't inherit
// pointerEvents, so every div sets it.
function Switch({ on }: { on: boolean }) {
  return (
    <div style={{ pointerEvents: "none", width: 34, height: 20, padding: 2, borderRadius: 9999, backgroundColor: on ? C.accent : "#343a49" }}>
      <div style={{ pointerEvents: "none", width: 16, height: 16, borderRadius: 9999, backgroundColor: on ? C.bg : "#c9ced8", marginLeft: on ? 14 : 0 }} />
    </div>
  )
}

function SwitchRow({ label, detail, on, flag }: { label: string; detail?: string; on: boolean; flag: string }) {
  return <Row label={<Label label={label} detail={detail} />} control={<Switch on={on} />} onClick={() => send({ event: "toggle", flag })} />
}

function Radio({ selected }: { selected: boolean }) {
  return (
    <div style={{ pointerEvents: "none", width: 16, height: 16, flexShrink: 0, borderRadius: 9999, borderWidth: 1, borderColor: selected ? C.accent : "#4a5163", display: "flex", alignItems: "center", justifyContent: "center" }}>
      {selected ? <div style={{ pointerEvents: "none", width: 8, height: 8, borderRadius: 9999, backgroundColor: C.accent }} /> : null}
    </div>
  )
}

function Segmented({ options }: { options: { label: string; selected: boolean; onClick: () => void }[] }) {
  return (
    <div style={{ display: "flex", flexDirection: "row", padding: 2, gap: 2, borderRadius: 8, backgroundColor: C.bg, borderWidth: 1, borderColor: C.inputBorder }}>
      {options.map((o) => (
        <div
          key={o.label}
          onClick={o.onClick}
          style={{
            flexGrow: 1, flexBasis: 0, height: 26, paddingLeft: 8, paddingRight: 8, display: "flex", alignItems: "center", justifyContent: "center",
            borderRadius: 6, cursor: "pointer",
            ...(o.selected ? { backgroundColor: C.raised } : {}),
          }}
        >
          <text style={{ fontSize: 12.5, whiteSpace: "nowrap", color: o.selected ? C.text : C.muted, fontWeight: o.selected ? 500 : 400 }}>{o.label}</text>
        </div>
      ))}
    </div>
  )
}

/**
 * Keeps what is typed locally while focused, so a view answering an older
 * keystroke can't rewind the field.
 */
function TextField({ field, value, placeholder, width, secret, autoFocus }: { field: Field; value: string; placeholder: string; width?: number; secret?: boolean; autoFocus?: boolean }) {
  const [local, setLocal] = useState(value)
  const [focused, setFocused] = useState(false)
  const [reveal, setReveal] = useState(false)
  const { renderer } = useGpuix()
  // Handlers can outlive a render, so they read focus from a ref.
  const hasFocus = useRef(false)
  useEffect(() => {
    if (!focused) setLocal(value)
  }, [value, focused])
  const box = {
    width: width ?? "100%", height: 30, paddingLeft: 10, paddingRight: 10, display: "flex", flexDirection: "row", alignItems: "center", gap: 8,
    borderRadius: 8, backgroundColor: C.bg, borderWidth: 1, borderColor: focused ? C.accent : C.inputBorder,
  } as const
  const masked = secret && !reveal && !focused
  return (
    // Clicking anywhere else ends editing, which also masks the key again.
    // On the box: `<input>` doesn't take the generic mouse events.
    <div style={box} onMouseDownOutside={() => { if (hasFocus.current) renderer?.blur?.() }}>
      {masked ? (
        <div onClick={() => setFocused(true)} style={{ flexGrow: 1, minWidth: 0, height: "100%", display: "flex", alignItems: "center", cursor: "text" }}>
          <text style={{ fontSize: 13, color: local ? C.text : C.faint, whiteSpace: "nowrap" }}>
            {local ? "•".repeat(Math.min(16, [...local].length)) : placeholder}
          </text>
        </div>
      ) : (
        <input
          value={local}
          placeholder={placeholder}
          autoFocus={autoFocus || (secret && focused)}
          onFocus={() => { hasFocus.current = true; setFocused(true); editing = field; send({ event: "editing" }) }}
          onBlur={() => { hasFocus.current = false; setFocused(false); if (editing === field) editing = null }}
          onChange={(e) => {
            const next = e.value ?? ""
            setLocal(next)
            send({ event: "set", field, value: next })
          }}
          style={{ flexGrow: 1, minWidth: 0, fontSize: 13, color: C.text }}
        />
      )}
      {secret && local ? (
        <div onMouseDown={() => setReveal(!reveal)} style={{ flexShrink: 0, cursor: "pointer" }}>
          <text style={{ fontSize: 12, color: C.muted, hover: { color: C.text } }}>{reveal ? "Hide" : "Show"}</text>
        </div>
      ) : null}
    </div>
  )
}

function Keycap({ view }: { view: SettingsView }) {
  const capturing = view.capturing
  return (
    <div
      onClick={() => send({ event: capturing ? "cancelCapture" : "capture" })}
      style={{
        height: 30, minWidth: 96, paddingLeft: 12, paddingRight: 12, display: "flex", alignItems: "center", justifyContent: "center",
        borderRadius: 8, borderWidth: 1, borderBottomWidth: 2, cursor: "pointer",
        ...(capturing
          ? { backgroundColor: "#8da2fb1f", borderColor: C.accent }
          : { backgroundColor: C.raised, borderColor: "#363d4e", hover: { borderColor: C.accent } }),
      }}
    >
      <text style={{ fontSize: 13, fontWeight: 500, color: capturing ? C.accent : C.text }}>{capturing ? "Press a key…" : view.keyLabel}</text>
    </div>
  )
}

function Settings({ initial }: { initial: SettingsView }) {
  const [view, setView] = useState(initial)
  const [focusModel, setFocusModel] = useState(false)
  useEffect(() => {
    update = setView
    if (latest) setView(latest)
  }, [])
  const hold = view.mode !== "toggle"
  const keyDetail = view.capturing ? "Esc to cancel" : view.keyNote
  const missingKey = !view.apiKey.trim()

  return (
    <div style={{ width: "100%", height: "100%", backgroundColor: C.bg }}>
      <div style={{ width: "100%", height: "100%", overflowY: "scroll" }}>
        <div style={{ display: "flex", flexDirection: "column", gap: 12, paddingLeft: 20, paddingRight: 20, paddingTop: 14, paddingBottom: 14 }}>
          <div style={{ display: "flex", flexDirection: "row", alignItems: "center", justifyContent: "space-between" }}>
            <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
              <text style={{ fontSize: 17, fontWeight: 600, color: C.text }}>Dictation</text>
              <text style={{ fontSize: 12.5, color: C.muted }}>{view.hint}</text>
            </div>
            <div
              onClick={() => process.exit(0)}
              style={{ height: 30, paddingLeft: 14, paddingRight: 14, display: "flex", alignItems: "center", borderRadius: 8, backgroundColor: C.accent, cursor: "pointer", hover: { backgroundColor: "#a3b4fc" } }}
            >
              <text style={{ fontSize: 13, fontWeight: 600, color: C.bg }}>Done</text>
            </div>
          </div>

          {view.banner ? (
            <div style={{ paddingLeft: 12, paddingRight: 12, paddingTop: 9, paddingBottom: 9, borderRadius: 10, backgroundColor: "#f8717126", borderWidth: 1, borderColor: "#f8717166" }}>
              <text style={{ fontSize: 12.5, lineHeight: 17, color: "#fecaca" }}>{view.banner}</text>
            </div>
          ) : null}

          <Section title="SHORTCUT">
            {[
              <Row key="key" label={<Label label="Key" detail={keyDetail} detailColor={!view.capturing && view.keyNoteError ? C.red : C.muted} />} control={<Keycap view={view} />} />,
              <Row
                key="mode"
                label={<Label label="Mode" />}
                control={
                  <div style={{ width: 160 }}>
                    <Segmented
                      options={[
                        { label: "Hold", selected: hold, onClick: () => send({ event: "mode", value: "hold" }) },
                        { label: "Toggle", selected: !hold, onClick: () => send({ event: "mode", value: "toggle" }) },
                      ]}
                    />
                  </div>
                }
              />,
            ]}
          </Section>

          <Section title="MICROPHONE">
            {view.mics.map((mic, i) => (
              <div
                key={i}
                onClick={() => send({ event: "mic", value: mic.value })}
                style={{ display: "flex", flexDirection: "row", alignItems: "center", gap: 12, height: 38, paddingLeft: 14, paddingRight: 14, cursor: "pointer", hover: { backgroundColor: C.hover } }}
              >
                <Radio selected={mic.selected} />
                <text style={{ flexShrink: 0, fontSize: 13.5, color: C.text }}>{mic.label}</text>
                <text style={{ minWidth: 0, fontSize: 12.5, color: C.muted, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{mic.detail}</text>
              </div>
            ))}
          </Section>

          <Section title="TRANSCRIPTION">
            {[
              view.envKey ? (
                <Row key="key" label={<Label label="API key" />} control={<text style={{ fontSize: 12.5, color: C.muted }}>From OPENROUTER_API_KEY</text>} />
              ) : (
                <Row
                  key="key"
                  label={
                    <Label label="API key">
                      {missingKey ? (
                        <div onClick={() => send({ event: "openKeys" })} style={{ cursor: "pointer" }}>
                          <text style={{ fontSize: 12, color: C.accent }}>Get a key ↗</text>
                        </div>
                      ) : null}
                    </Label>
                  }
                  control={<TextField field="apiKey" value={view.apiKey} placeholder="sk-or-…" width={220} secret />}
                />
              ),
              <div key="model" style={{ display: "flex", flexDirection: "column" }}>
                <Row
                  label={<Label label="Model" />}
                  control={
                    <div style={{ width: 300 }}>
                      <Segmented
                        options={[
                          ...view.models.map((m) => ({
                            label: m.label,
                            selected: !view.customModel && view.model === m.slug,
                            onClick: () => send({ event: "preset", slug: m.slug }),
                          })),
                          { label: "Custom", selected: view.customModel, onClick: () => { setFocusModel(true); send({ event: "custom" }) } },
                        ]}
                      />
                    </div>
                  }
                />
                {view.customModel ? (
                  <div style={{ paddingLeft: 14, paddingRight: 14, paddingBottom: 10 }}>
                    <TextField field="model" value={view.model} placeholder="provider/model" autoFocus={focusModel} />
                  </div>
                ) : null}
              </div>,
              <Row key="lang" label={<Label label="Language" />} control={<TextField field="language" value={view.language} placeholder="Auto-detect" width={120} />} />,
              <SwitchRow key="cleanup" label="Clean up transcript" detail="Fix punctuation, filler words, and misheard terms" on={view.cleanup} flag="cleanup" />,
            ]}
          </Section>

          <Section title="OUTPUT">
            {[
              <SwitchRow key="type" label="Type into focused window" detail="The transcript is always copied to the clipboard too" on={view.typeText} flag="typeText" />,
              <SwitchRow key="beeps" label="Sound cues" on={view.beeps} flag="beeps" />,
            ]}
          </Section>
        </div>
      </div>
    </div>
  )
}

export function runSettings(): void {
  void listen<SettingsView>((view) => {
    const opened = latest != null
    latest = view
    if (update) return update(view)
    if (opened) return
    render(<Settings initial={view} />, {
      title: "Dictation Settings",
      appId: "dictationapp",
      width: 460,
      height: 760,
      resizable: false,
      tabNavigation: true,
      onKeyDown: (event, renderer) => {
        if (event.key !== "escape") return
        if (latest?.capturing) send({ event: "cancelCapture" })
        else if (editing) renderer.blur?.()
        else process.exit(0)
      },
    })
  })
}
