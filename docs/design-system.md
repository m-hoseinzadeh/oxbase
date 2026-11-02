# Oxbase design system

Oxbase is a desktop database client: people spend hours in it reading dense tables, SQL and
schema trees. The design serves that work. It is calm, flat and dense, and the data stays in
front. These rules apply to everything under `ui/`. The tokens are in `ui/src/theme.css`.

Source: the `ui-ux-pro-max` recommendations for a developer tool (dark-first, slate surfaces,
"run" green accent, Inter + a monospace face, flat data-dense layout), adapted from its mobile
defaults to a desktop pointer-and-keyboard app.

## Principles

1. **Data first.** Chrome is quiet (slate surfaces, thin borders, no gradients or glow). Colour
   is spent on meaning: run/primary actions, selection, errors, engine identity.
2. **Dense but readable.** 13 px UI text, 28 px controls, 4 px spacing grid. Long-form text is
   rare; tables and code use a monospace face with tabular numbers.
3. **Keyboard complete.** Every action has a visible control, and frequent ones have shortcuts
   shown in tooltips (`Ctrl+Enter` Run). Focus is always visible.
4. **Both themes are first-class.** Dark is the default; light is designed, not inverted. The
   user picks Dark, Light or System in the status bar.
5. **No layout shift.** Hover and press change colour only; loading reserves space (skeletons,
   fixed toolbars); numbers use tabular figures.

## Colour

Semantic tokens only; components never use raw hex values. Ratios are against `--color-bg`
unless noted; text pairs meet 4.5:1 on every surface they appear on, control outlines 3:1.

| Token | Dark | Light | Use |
|---|---|---|---|
| `--color-bg` | `#0F172A` | `#FFFFFF` | Editors, grids, panel bodies |
| `--color-bg-subtle` | `#131C2F` | `#F8FAFC` | Side panels, toolbars, tab strips |
| `--color-bg-muted` | `#1E293B` | `#F1F5F9` | Hover, zebra rows, inactive chips |
| `--color-bg-elevated` | `#172036` | `#FFFFFF` | Dialogs, menus, popovers |
| `--color-fg` | `#E2E8F0` (14.5:1) | `#0F172A` (17.9:1) | Primary text |
| `--color-fg-muted` | `#94A3B8` (6.9:1) | `#475569` (7.6:1) | Secondary text, labels |
| `--color-fg-faint` | `#8A99AE` (6.2:1; 5.1:1 on muted) | `#5B6B80` (5.0:1 on muted) | Hints, placeholders, meta |
| `--color-border` | `#243045` | `#E2E8F0` | Dividers, panel edges |
| `--color-border-strong` | `#334155` | `#CBD5E1` | Section dividers, table header edges |
| `--color-border-control` | `#64748B` (3.4:1+) | `#7C8BA1` (3.3:1+) | Input, select and button outlines (WCAG 1.4.11) |
| `--color-accent` | `#22C55E` | `#15803D` | Primary / run actions |
| `--color-accent-fg` | `#052E16` | `#FFFFFF` | Text on accent |
| `--color-focus` | `#38BDF8` | `#0284C7` | Focus rings, active tab, selection edge |
| `--color-selection` | sky 18% | sky 14% | Selected rows and tree items |
| `--color-danger` | `#F87171` | `#DC2626` | Errors, destructive actions |
| `--color-warning` | `#FBBF24` | `#B45309` | Warnings, non-transactional notes |
| `--color-success` | `#4ADE80` | `#15803D` | Success, connected state |

Rules:
- Green means "do it" (Run, Connect, Apply, Import). One primary button per toolbar or dialog.
- Blue means "where you are" (focus, selection, active tab). It is never a button fill.
- Status colours always come with an icon or text (`color-not-only`).
- Engine identity colours (`--engine-*`) appear only in the small engine tile.

## Typography

| Token | Value | Use |
|---|---|---|
| `--font-ui` | Inter Variable, system fallbacks | All interface text |
| `--font-mono` | JetBrains Mono Variable, system fallbacks | SQL editor, grid cells, DDL, values |
| `--text-xs` / `--text-sm` / `--text-md` / `--text-lg` / `--text-xl` / `--text-2xl` | 11 / 12 / 13 / 14 / 16 / 22 px | Meta / secondary / body / emphasis / section / page title |

Weights: 400 body, 500 labels and tabs, 600 headings and names. Line height 1.45 for UI text.
Both fonts are bundled (`@fontsource-variable/*`, OFL-1.1); the app's CSP allows no font CDN.
13 px body text is a deliberate desktop choice (the mobile 16 px rule does not apply); nothing
interactive is smaller than 12 px.

## Space, size, shape

- Spacing: `--space-1..8` = 4, 8, 12, 16, 20, 24, 32 px (4 px grid).
- Control heights: `--control-sm` 24 px, `--control-md` 28 px, `--control-lg` 32 px. Pointer
  targets are at least 24 × 24 px (WCAG 2.2 target size); icon buttons are 28 × 28 px.
- Radius: `--radius-sm` 4 px (inputs, chips), `--radius-md` 6 px (buttons, menus),
  `--radius-lg` 10 px (dialogs, cards).
- Elevation: flat by default. `--shadow-popover` for menus and completion lists,
  `--shadow-dialog` for dialogs. Nothing else casts a shadow.
- Z-index: `--z-sticky` 10, `--z-dropdown` 100, `--z-popover` 200, `--z-modal` 1000,
  `--z-toast` 1100, `--z-tooltip` 1200.

## Motion

`--duration-fast` 120 ms (hover, press), `--duration` 180 ms (menus, dialogs, panels), ease-out
on enter, faster ease-in on exit. Only `opacity`, `transform`, `color`, `background-color` and
`border-color` animate. `prefers-reduced-motion: reduce` sets both durations to 0.

## Icons

Lucide (`lucide-react`, ISC) only: 16 px in toolbars and trees, 14 px inline, stroke 1.75.
Icon-only buttons carry `aria-label` and a `title` with the shortcut. No emoji, no letters in
place of icons except the engine tile.

## Components

| Component | Class / file | Notes |
|---|---|---|
| Button | `.button`, `--primary`, `--danger`, `--ghost`, `--small` | Icon + label; primary is green |
| Icon button | `.icon-button` | 28 × 28, ghost; `aria-label` required |
| Input, select, textarea | element styles in `theme.css` | 28 px, strong border, focus ring |
| Tabs | `.tabs`, `.tabs__tab` | Blue underline on the active tab |
| Badge / chip | `.badge`, `--success`, `--danger`, `--warning`, `--info` | Status with icon or text |
| Keyboard hint | `.kbd` | Shortcuts in tooltips and menus |
| Empty state | `.empty-state` + icon | Title, one hint, one action |
| Skeleton | `.skeleton` | Loading > 300 ms reserves the final shape |
| Modal | `components/Modal.tsx` | Elevated surface, 50 % scrim, Escape closes |
| Engine tile | `.engine-badge--<engine>` | The only place engine colours appear |

## Layout

```
┌ app bar: mark · New connection · New query · Import · Export · History ─────── theme ┐
├ side (Connections / Objects) ┬ editor area (dockview tabs) ───────────────────────────┤
│                              │ query editor (Monaco) · results grid (Glide)            │
│                              ├ bottom: History / Messages ──────────────────────────────┤
├ status bar: core · open connections · active session ───────────────── Dark/Light/System ┤
```

The dockview layout, Monaco and the Glide grid take their colours from these tokens
(`workbench/theme-bridge.ts`), so switching theme restyles all three without reloading.
