# oxbase UI

React + TypeScript (strict) front end for the oxbase desktop app. It runs inside the Tauri 2
shell in [`../crates/app-tauri`](../crates/app-tauri). All database logic lives in the Rust
core; this package only renders and calls Tauri commands.

Stack: pnpm, Vite, React 19, TypeScript, Biome (lint + format), Vitest + Testing Library,
dockview (layout), `@tauri-apps/api` v2, `apache-arrow` (decodes result pages).

## Prerequisites

- Node 22.12+ and pnpm (developed with Node 24 and pnpm 12).
- Rust stable (the repo's `rust-toolchain.toml` selects it).
- Tauri system libraries for desktop builds. On Debian/Ubuntu:
  `sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev librsvg2-dev build-essential`.
  See <https://v2.tauri.app/start/prerequisites/> for macOS and Windows.

## Commands

Run everything from this `ui/` folder.

| Command                 | What it does                                                                                                     |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `pnpm install`          | Install JS dependencies                                                                                          |
| `pnpm dev`              | Vite dev server on http://localhost:5173 (browser preview; the core is not available, so the status bar says so) |
| `pnpm build`            | Type-check, then build to `ui/dist` (what the Tauri bundle embeds)                                               |
| `pnpm lint`             | Biome lint + format check (`pnpm format` rewrites files)                                                         |
| `pnpm typecheck`        | `tsc -b` over `tsconfig.app.json` (browser code) and `tsconfig.node.json` (tests, `vite.config.ts`)              |
| `pnpm test`             | Vitest component tests (jsdom)                                                                                   |
| `pnpm tauri dev`        | Desktop app with hot reload: starts `pnpm dev`, then compiles and runs `app-tauri`                               |
| `pnpm tauri build`      | Release bundle for the current OS (runs `pnpm build` first)                                                      |
| `pnpm tauri icon <png>` | Regenerate `crates/app-tauri/icons/` from a 1024x1024 PNG                                                        |

`pnpm tauri ...` goes through `scripts/tauri.mjs`, which points the Tauri CLI at
`../crates/app-tauri` (`TAURI_APP_PATH`) and this folder (`TAURI_FRONTEND_PATH`), because the
Rust shell is not inside `ui/`.

## Layout

- `src/App.tsx`: dockview layout. Left "Connections" panel, main "Welcome" tab, status bar.
- `src/ipc.ts`: typed wrappers for the Tauri commands `app_info` and `demo_arrow_page`.
  Result pages arrive as raw Arrow IPC stream bytes (`tauri::ipc::Response`) and are decoded
  with `apache-arrow`, never as JSON.
- `src/theme.css`: design tokens as CSS variables. Light by default, dark under
  `prefers-color-scheme: dark`. dockview colours are mapped onto the same tokens.
- `src/test/fixtures/demo-page.arrows`: the bytes the Rust `demo_arrow_page` command returns
  (written by arrow-rs). The Welcome test decodes it, so it checks the Rust → JS format path.
  `cargo test -p app-tauri` fails if it drifts from `crates/app-tauri/src/demo_page.rs`;
  regenerate it with `OXBASE_REGEN_UI_FIXTURES=1 cargo test -p app-tauri`.
- TypeScript configs: `tsconfig.app.json` covers browser code in `src/` without Node types;
  `tsconfig.node.json` covers tests, `src/test/` and `vite.config.ts` with Node types. Both extend
  `tsconfig.base.json`. `apache-arrow`'s typings reference `@types/node`, so the type checker
  cannot keep Node globals out of app code alone. A Biome override (`noNodejsModules`,
  `noProcessGlobal`, `noGlobalDirnameFilename`) rejects them in non-test `src/` files.

## Icons

`crates/app-tauri/icons/app-icon.png` is a generated placeholder (blue tile with a database
cylinder), not a final brand asset. The other files there come from `pnpm tauri icon`.
Mobile (Android/iOS) icon sets were removed because v1 targets desktop only.
