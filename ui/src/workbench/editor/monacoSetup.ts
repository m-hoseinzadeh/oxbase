/**
 * Monaco, bundled locally (the CSP allows only same-origin scripts; there is no CDN).
 *
 * Only the core editor, its standard contributions, the three SQL grammars and (M7) the
 * JavaScript grammar of the mongo shell and the Redis console grammar are loaded, plus the one
 * web worker the core needs (no language-service workers: the grammars are Monarch tokenizers). Vite emits the worker and the codicon font as same-origin
 * files, so no `blob:` or `data:` script source is needed.
 */
import * as monaco from "monaco-editor/editor";
import "monaco-editor/features/register.all";
import "monaco-editor/languages/definitions/pgsql/register";
import "monaco-editor/languages/definitions/mysql/register";
import "monaco-editor/languages/definitions/sql/register";
import "monaco-editor/languages/definitions/javascript/register";
import "monaco-editor/languages/definitions/redis/register";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";

globalThis.MonacoEnvironment = {
  getWorker: () => new EditorWorker(),
};

export { monaco };
