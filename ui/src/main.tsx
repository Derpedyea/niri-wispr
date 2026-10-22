// dictationapp-ui <pill|settings>: one window per process, driven by the
// dictationapp daemon over stdin/stdout (see link.ts).

import { runPill } from "./pill"
import { runSettings } from "./settings"

switch (process.argv[2]) {
  case "pill":
    runPill()
    break
  case "settings":
    runSettings()
    break
  default:
    console.error("usage: dictationapp-ui <pill|settings>  (started by dictationapp)")
    process.exit(2)
}
