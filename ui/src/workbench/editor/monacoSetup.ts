/**
 * Monaco, bundled locally (the CSP allows only same-origin scripts; there is no CDN).
 *
 * Only the core editor, its standard contributions and the three SQL grammars are loaded, plus
 * the one web worker the core needs. Vite emits the worker and the codicon font as same-origin
 * files, so no `blob:` or `data:` script source is needed.
 */
import * as monaco from "monaco-editor/editor";
import "monaco-editor/features/register.all";
import "monaco-editor/languages/definitions/pgsql/register";
import "monaco-editor/languages/definitions/mysql/register";
import "monaco-editor/languages/definitions/sql/register";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";

globalThis.MonacoEnvironment = {
  getWorker: () => new EditorWorker(),
};

export { monaco };
