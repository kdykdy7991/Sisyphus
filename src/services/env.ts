// Runtime environment detection. Under `tauri dev`/bundled the injected
// `__TAURI_INTERNALS__` global is present; under a plain `npm run dev` it is
// not, so the resolver falls back to the Mock implementations.
export const isTauri =
  typeof (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ !== 'undefined';