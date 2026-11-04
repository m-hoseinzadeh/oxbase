import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useSyncExternalStore } from "react";

/**
 * The user's theme choice and the theme in effect. The CSS tokens in theme.css switch on
 * `<html data-theme>`; canvas and editor surfaces (Glide grid, Monaco) cannot read CSS
 * variables live, so they subscribe here and re-read the tokens when the theme changes.
 * What the OS draws for the webview (native `<select>` popups, the webview's own context
 * menu, scrollbars on some platforms) follows the native window's theme, so the choice is
 * passed to Tauri too ("system" lets the window follow the OS). See docs/design-system.md.
 */
export type ThemePreference = "dark" | "light" | "system";
export type ResolvedTheme = "dark" | "light";

const STORAGE_KEY = "oxbase.theme";
const DEFAULT: ThemePreference = "dark";
const listeners = new Set<() => void>();
let preference: ThemePreference = readStored();
let media: MediaQueryList | null = null;

function readStored(): ThemePreference {
  try {
    const value = globalThis.localStorage?.getItem(STORAGE_KEY);
    if (value === "dark" || value === "light" || value === "system") return value;
  } catch {
    // Storage can be unavailable (private mode, tests); the default applies.
  }
  return DEFAULT;
}

function systemPrefersLight(): boolean {
  return globalThis.matchMedia?.("(prefers-color-scheme: light)").matches ?? false;
}

function notify() {
  // One failing surface (e.g. Monaco rejecting a colour) must not stop the others re-theming.
  for (const listener of listeners) {
    try {
      listener();
    } catch (error) {
      console.error("theme listener failed", error);
    }
  }
}

function apply() {
  if (typeof document !== "undefined") document.documentElement.dataset.theme = preference;
  applyNative();
}

/** The native window's theme: the choice, or `null` (follow the OS) for "system". */
export function nativeTheme(choice: ThemePreference): "dark" | "light" | null {
  return choice === "system" ? null : choice;
}

function applyNative() {
  // Outside the app (tests, a plain browser) there is no native window.
  if (!isTauri()) return;
  try {
    getCurrentWindow()
      .setTheme(nativeTheme(preference))
      .catch((error: unknown) => console.warn("could not set the window theme", error));
  } catch (error) {
    console.warn("could not set the window theme", error);
  }
}

/** Applies the stored choice to `<html>` and follows OS changes while "system" is chosen. */
export function initTheme(): void {
  apply();
  if (media || typeof globalThis.matchMedia !== "function") return;
  media = globalThis.matchMedia("(prefers-color-scheme: light)");
  media.addEventListener("change", () => {
    if (preference === "system") notify();
  });
}

export function getThemePreference(): ThemePreference {
  return preference;
}

export function setThemePreference(next: ThemePreference): void {
  if (next === preference) return;
  preference = next;
  try {
    globalThis.localStorage?.setItem(STORAGE_KEY, next);
  } catch {
    // Not persisted; the choice still applies for this session.
  }
  apply();
  notify();
}

export function resolvedTheme(): ResolvedTheme {
  if (preference === "system") return systemPrefersLight() ? "light" : "dark";
  return preference;
}

/** Called after the theme in effect may have changed (choice or OS setting). */
export function subscribeTheme(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useThemePreference(): ThemePreference {
  return useSyncExternalStore(subscribeTheme, getThemePreference, () => DEFAULT);
}

export function useResolvedTheme(): ResolvedTheme {
  return useSyncExternalStore(subscribeTheme, resolvedTheme, () => "dark");
}

/** A CSS custom property of `<html>` as a resolved string, or `fallback` when unset. */
export function cssToken(name: string, fallback: string): string {
  if (typeof document === "undefined") return fallback;
  const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return value || fallback;
}

// ------------------------------------------------------------------------------- Monaco

/** The structural subset of the Monaco API used here (keeps this module test-friendly). */
export interface MonacoThemeApi {
  editor: {
    defineTheme(name: string, data: unknown): void;
    setTheme(name: string): void;
  };
}

/** Syntax colours per theme; all reach 4.5:1 on `--color-bg` (comments 4.7:1+). */
const SYNTAX: Record<ResolvedTheme, Record<string, string>> = {
  dark: {
    keyword: "93C5FD",
    string: "86EFAC",
    number: "FCD34D",
    comment: "8A99AE",
    predefined: "67E8F9",
    type: "C4B5FD",
    operator: "94A3B8",
  },
  light: {
    keyword: "1D4ED8",
    string: "15803D",
    number: "B45309",
    comment: "5B6B80",
    predefined: "0E7490",
    type: "6D28D9",
    operator: "475569",
  },
};

/**
 * A colour as `#rrggbb` or `#rrggbbaa`. Built CSS is minified, so tokens can come back as
 * `#fff`, `#0284c733` or `rgb(2 132 199 / 20%)`; Monaco accepts only the long hex forms.
 * Returns `null` for anything else (named colours, `var()`), so callers can fall back.
 */
export function normalizeColor(value: string): string | null {
  const v = value.trim().toLowerCase();
  const short = /^#([0-9a-f])([0-9a-f])([0-9a-f])([0-9a-f])?$/.exec(v);
  if (short) {
    const [r, g, b, a] = [short[1], short[2], short[3], short[4]];
    return `#${r}${r}${g}${g}${b}${b}${a ? `${a}${a}` : ""}`;
  }
  if (/^#[0-9a-f]{6}([0-9a-f]{2})?$/.test(v)) return v;
  const fn =
    /^rgba?\(\s*([\d.]+%?)[\s,]+([\d.]+%?)[\s,]+([\d.]+%?)(?:\s*[,/]\s*([\d.]+%?))?\s*\)$/.exec(v);
  if (!fn) return null;
  const channel = (c: string | undefined) =>
    c?.endsWith("%") ? Math.round((Number.parseFloat(c) / 100) * 255) : Number.parseFloat(c ?? "0");
  const alpha = (c: string | undefined) =>
    c === undefined
      ? 255
      : Math.round((c.endsWith("%") ? Number.parseFloat(c) / 100 : Number.parseFloat(c)) * 255);
  const part = (n: number) =>
    Math.max(0, Math.min(255, Math.round(n)))
      .toString(16)
      .padStart(2, "0");
  const a = alpha(fn[4]);
  return `#${part(channel(fn[1]))}${part(channel(fn[2]))}${part(channel(fn[3]))}${a === 255 ? "" : part(a)}`;
}

function hex(value: string, fallback: string): string {
  return normalizeColor(value) ?? fallback;
}

export function monacoThemeName(theme: ResolvedTheme = resolvedTheme()): string {
  return theme === "light" ? "oxbase-light" : "oxbase-dark";
}

/** (Re)defines the Oxbase Monaco theme for the theme in effect and activates it. */
export function applyMonacoTheme(monaco: MonacoThemeApi): void {
  const theme = resolvedTheme();
  const s = SYNTAX[theme];
  const rule = (token: string, color: string | undefined, fontStyle?: string) => ({
    token,
    ...(color ? { foreground: color } : {}),
    ...(fontStyle ? { fontStyle } : {}),
  });
  monaco.editor.defineTheme(monacoThemeName(theme), {
    base: theme === "light" ? "vs" : "vs-dark",
    inherit: true,
    rules: [
      rule("keyword", s.keyword, "bold"),
      rule("string", s.string),
      rule("number", s.number),
      rule("comment", s.comment, "italic"),
      rule("predefined", s.predefined),
      rule("type", s.type),
      rule("operator", s.operator),
      rule("delimiter", s.operator),
    ],
    colors: {
      "editor.background": hex(cssToken("--color-bg", "#0f172a"), "#0f172a"),
      "editor.foreground": hex(cssToken("--color-fg", "#e2e8f0"), "#e2e8f0"),
      "editorLineNumber.foreground": hex(cssToken("--color-fg-faint", "#8a99ae"), "#8a99ae"),
      "editorLineNumber.activeForeground": hex(cssToken("--color-fg", "#e2e8f0"), "#e2e8f0"),
      "editor.lineHighlightBackground": hex(cssToken("--color-bg-subtle", "#131c2f"), "#131c2f"),
      "editor.lineHighlightBorder": "#00000000",
      "editor.selectionBackground": hex(
        cssToken("--color-selection-strong", "#38bdf847"),
        "#38bdf847",
      ),
      "editor.inactiveSelectionBackground": hex(
        cssToken("--color-selection", "#38bdf82e"),
        "#38bdf82e",
      ),
      "editorCursor.foreground": hex(cssToken("--color-focus", "#38bdf8"), "#38bdf8"),
      "editorIndentGuide.background1": hex(cssToken("--color-border", "#243045"), "#243045"),
      "editorWidget.background": hex(cssToken("--color-bg-elevated", "#172036"), "#172036"),
      "editorWidget.border": hex(cssToken("--color-border-strong", "#334155"), "#334155"),
      "editorSuggestWidget.background": hex(cssToken("--color-bg-elevated", "#172036"), "#172036"),
      "editorSuggestWidget.border": hex(cssToken("--color-border-strong", "#334155"), "#334155"),
      "editorSuggestWidget.selectedBackground": hex(
        cssToken("--color-selection-strong", "#38bdf847"),
        "#38bdf847",
      ),
      "editorHoverWidget.background": hex(cssToken("--color-bg-elevated", "#172036"), "#172036"),
      "editorHoverWidget.border": hex(cssToken("--color-border-strong", "#334155"), "#334155"),
      "editorError.foreground": hex(cssToken("--color-danger", "#f87171"), "#f87171"),
      "editorWarning.foreground": hex(cssToken("--color-warning", "#fbbf24"), "#fbbf24"),
      // The editor's own context menu (Cut / Copy / Paste / Command Palette).
      "menu.background": hex(cssToken("--color-bg-elevated", "#172036"), "#172036"),
      "menu.foreground": hex(cssToken("--color-fg", "#e2e8f0"), "#e2e8f0"),
      "menu.selectionBackground": hex(cssToken("--color-bg-muted", "#1e293b"), "#1e293b"),
      "menu.selectionForeground": hex(cssToken("--color-fg", "#e2e8f0"), "#e2e8f0"),
      "menu.separatorBackground": hex(cssToken("--color-border", "#243045"), "#243045"),
      "menu.border": hex(cssToken("--color-border-strong", "#334155"), "#334155"),
      focusBorder: hex(cssToken("--color-focus", "#38bdf8"), "#38bdf8"),
    },
  });
  monaco.editor.setTheme(monacoThemeName(theme));
}
