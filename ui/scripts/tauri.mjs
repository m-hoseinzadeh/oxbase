// Runs the Tauri CLI against the shell crate in ../crates/app-tauri.
//
// The Rust shell lives outside ui/, so the CLI cannot discover it by walking the current
// folder. This wrapper points it at the right folders on every OS (no shell-specific env syntax).
import { fileURLToPath } from "node:url";
import { run } from "@tauri-apps/cli";

process.env.TAURI_APP_PATH ??= fileURLToPath(new URL("../../crates/app-tauri", import.meta.url));
process.env.TAURI_FRONTEND_PATH ??= fileURLToPath(new URL("..", import.meta.url));

await run(process.argv.slice(2), "pnpm tauri");
