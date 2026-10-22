// GPUIX loads its renderer with a dynamic require that `bun build --compile`
// can't follow. Embed the addon here and point napi-rs's loader at it; this
// module must be imported before anything from @gpuix. Compiled builds only:
// running sources, Bun refuses to import a .node file (see compiled.ts).

// @ts-expect-error: a native addon, imported as an embedded file path
import addon from "@gpuix/native-linux-x64-gnu/gpuix-native.linux-x64-gnu.node" with { type: "file" }

process.env.NAPI_RS_NATIVE_LIBRARY_PATH ??= addon
