/**
 * The Settings window (toolbar gear, Ctrl+,): appearance (theme, zoom, grid rows), table data
 * (rows to load, Filter & sort open), new connections (Sessions) and the keyboard shortcuts.
 * Every change applies at once and is remembered; there is nothing to save.
 */
import { useEffect, useId, useState } from "react";
import { Modal } from "../components/Modal";
import { Select } from "../components/Select";
import type { SessionMode } from "../connections/types";
import { type GridDensity, onOpenSettings, updateSettings, useSettings } from "../settings";
import {
  defaultRowLimit,
  parseRowLimit,
  ROW_LIMITS,
  rowLimitLabel,
  rowLimitValue,
  saveDefaultRowLimit,
} from "./data/rowLimit";
import { setThemePreference, type ThemePreference, useThemePreference } from "./theme-bridge";
import { setZoom, useZoom, ZOOM_LEVELS, zoomLabel } from "./zoom";

const THEMES: { value: ThemePreference; label: string }[] = [
  { value: "dark", label: "Dark" },
  { value: "light", label: "Light" },
  { value: "system", label: "Follow the system" },
];

const DENSITIES: { value: GridDensity; label: string }[] = [
  { value: "compact", label: "Compact (24 px rows)" },
  { value: "default", label: "Default (28 px rows)" },
  { value: "comfortable", label: "Comfortable (34 px rows)" },
];

const SESSIONS: { value: SessionMode; label: string }[] = [
  { value: "per_tab", label: "Keep a session per tab" },
  { value: "per_run", label: "Connect for each run" },
];

/** The keyboard shortcuts, as the Settings window lists them. */
export const SHORTCUT_GROUPS: { title: string; keys: [string, string][] }[] = [
  {
    title: "Everywhere",
    keys: [
      ["Ctrl+N", "New query tab (in a table's grid: add a row)"],
      ["Ctrl+O", "Open a .sql file in a query tab"],
      ["Ctrl+,", "Settings"],
      ["Ctrl+= / Ctrl+- / Ctrl+0", "Zoom in, out, back to 100%"],
    ],
  },
  {
    title: "Query tab",
    keys: [
      ["F5, Ctrl+R", "Run the selection, else the whole script"],
      ["Ctrl+Enter", "Run the selection, else the statement at the cursor"],
      ["Ctrl+Shift+Enter", "Run the whole script"],
      ["Shift+F5", "Stop the running query"],
      ["Ctrl+S", "Save (a new query: as a saved query of its connection)"],
      ["Ctrl+Shift+S", "Save as a file"],
    ],
  },
  {
    title: "Grid",
    keys: [
      ["Enter, Space, F2", "Edit the cell (read-only: show the value panel)"],
      ["Shift+Enter", "Open the value dialog"],
      ["Ctrl+C / Ctrl+Shift+C", "Copy / copy with column names"],
      ["Ctrl+F", "Search the grid"],
      ["Ctrl+A", "Select everything"],
      ["Ctrl+click, Shift+click", "Add to / extend the selection"],
    ],
  },
  {
    title: "Table data",
    keys: [
      ["F5", "Reload the rows"],
      ["Ctrl+V / Ctrl+Shift+V", "Paste into cells / as new rows"],
      ["Ctrl+D", "Duplicate the row"],
      ["Delete, Ctrl+Alt+-", "Mark the selected rows for deletion"],
      ["Ctrl+Z", "Revert the row"],
      ["Ctrl+S", "Apply the pending changes"],
    ],
  },
];

export function SettingsDialog({ onClose }: { onClose: () => void }) {
  const id = useId();
  const settings = useSettings();
  const theme = useThemePreference();
  const zoom = useZoom();
  const [rowLimit, setRowLimit] = useState(defaultRowLimit);

  return (
    <Modal
      title="Settings"
      onClose={onClose}
      className="modal--wide settings-dialog"
      footer={
        <button type="button" className="button button--primary" onClick={onClose}>
          Close
        </button>
      }
    >
      <div className="settings">
        <fieldset className="fieldset settings__group">
          <legend>Appearance</legend>
          <div className="field-row">
            <div className="field">
              <label htmlFor={`${id}-theme`}>Theme</label>
              <Select
                id={`${id}-theme`}
                value={theme}
                data-autofocus
                onChange={(e) => setThemePreference(e.target.value as ThemePreference)}
              >
                {THEMES.map((t) => (
                  <option key={t.value} value={t.value}>
                    {t.label}
                  </option>
                ))}
              </Select>
            </div>
            <div className="field">
              <label htmlFor={`${id}-zoom`}>Zoom</label>
              <Select
                id={`${id}-zoom`}
                value={String(zoom)}
                onChange={(e) => setZoom(Number(e.target.value))}
              >
                {ZOOM_LEVELS.map((level) => (
                  <option key={level} value={String(level)}>
                    {zoomLabel(level)}
                  </option>
                ))}
              </Select>
            </div>
          </div>
          <div className="field">
            <label htmlFor={`${id}-density`}>Grid rows</label>
            <Select
              id={`${id}-density`}
              value={settings.gridDensity}
              onChange={(e) => updateSettings({ gridDensity: e.target.value as GridDensity })}
            >
              {DENSITIES.map((d) => (
                <option key={d.value} value={d.value}>
                  {d.label}
                </option>
              ))}
            </Select>
          </div>
        </fieldset>

        <fieldset className="fieldset settings__group">
          <legend>Table data</legend>
          <div className="field">
            <label htmlFor={`${id}-rows`}>Rows to load when a table opens</label>
            <Select
              id={`${id}-rows`}
              value={rowLimitValue(rowLimit)}
              aria-describedby={`${id}-rows-hint`}
              onChange={(e) => {
                const limit = parseRowLimit(e.target.value);
                setRowLimit(limit);
                saveDefaultRowLimit(limit);
              }}
            >
              {ROW_LIMITS.map((l) => (
                <option key={rowLimitValue(l)} value={rowLimitValue(l)}>
                  {rowLimitLabel(l)}
                </option>
              ))}
            </Select>
            <p className="field__hint" id={`${id}-rows-hint`}>
              Each tab can still load more with its Rows control. Fewer rows open faster and hold
              the table for less time.
            </p>
          </div>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={settings.filterOpen}
              onChange={(e) => updateSettings({ filterOpen: e.target.checked })}
            />
            Show Filter &amp; sort when a table opens
          </label>
        </fieldset>

        <fieldset className="fieldset settings__group">
          <legend>New connections</legend>
          <div className="field">
            <label htmlFor={`${id}-sessions`}>Sessions</label>
            <Select
              id={`${id}-sessions`}
              value={settings.sessionMode}
              aria-describedby={`${id}-sessions-hint`}
              onChange={(e) => updateSettings({ sessionMode: e.target.value as SessionMode })}
            >
              {SESSIONS.map((s) => (
                <option key={s.value} value={s.value}>
                  {s.label}
                </option>
              ))}
            </Select>
            <p className="field__hint" id={`${id}-sessions-hint`}>
              What a new PostgreSQL, MySQL, MariaDB or SQL Server connection starts with. Each
              connection can change it in its settings.
            </p>
          </div>
        </fieldset>

        <fieldset className="fieldset settings__group">
          <legend>Quitting</legend>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={settings.confirmQuit}
              onChange={(e) => updateSettings({ confirmQuit: e.target.checked })}
            />
            Ask before quitting Oxbase
          </label>
        </fieldset>

        <section className="settings__group settings__shortcuts" aria-labelledby={`${id}-keys`}>
          <h3 className="settings__heading" id={`${id}-keys`}>
            Keyboard shortcuts
          </h3>
          <p className="field__hint">On macOS, Cmd stands for Ctrl.</p>
          {SHORTCUT_GROUPS.map((group) => (
            <table key={group.title} className="shortcut-table">
              <caption>{group.title}</caption>
              <tbody>
                {group.keys.map(([keys, action]) => (
                  <tr key={keys}>
                    <th scope="row">
                      <kbd className="kbd">{keys}</kbd>
                    </th>
                    <td>{action}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          ))}
        </section>
      </div>
    </Modal>
  );
}

/** Shows the Settings window when it is asked for ({@link onOpenSettings}). */
export function SettingsHost() {
  const [open, setOpen] = useState(false);
  useEffect(() => onOpenSettings(() => setOpen(true)), []);
  return open ? <SettingsDialog onClose={() => setOpen(false)} /> : null;
}
