/**
 * Wires {@link InsightController} into Monaco. Providers are registered once per SQL grammar
 * (`pgsql`, `mysql`, `sql`) and look up the controller of the model they are asked about, so
 * editors without insight (read-only DDL previews) get nothing.
 */
import { monaco } from "../monacoSetup";
import {
  type FormatOutcome,
  InsightController,
  type InsightControllerOptions,
  type MarkerSink,
} from "./controller";
import { MARKER_OWNER, type MarkerLike, type MonacoEnums } from "./mapping";

const ENUMS: MonacoEnums = {
  CompletionItemKind: monaco.languages.CompletionItemKind,
  CompletionItemInsertTextRule: monaco.languages.CompletionItemInsertTextRule,
  MarkerSeverity: monaco.MarkerSeverity,
};

/** The Monaco grammars insight serves (see `../language.ts`). */
const LANGUAGES = ["pgsql", "mysql", "sql"] as const;

/**
 * Characters that open the list without a word typed: `.` (qualified names), the identifier
 * quotes, and a space (names after `FROM `, `JOIN `, `SELECT `; keywords are not offered there).
 */
export const TRIGGER_CHARACTERS = [".", " ", '"', "`", "["];

const controllers = new Map<string, InsightController>();
let registered = false;

function controllerOf(model: monaco.editor.ITextModel): InsightController | undefined {
  return controllers.get(model.uri.toString());
}

function registerProviders() {
  if (registered) return;
  registered = true;
  for (const language of LANGUAGES) {
    monaco.languages.registerCompletionItemProvider(language, {
      triggerCharacters: TRIGGER_CHARACTERS,
      provideCompletionItems: (model, position, context, token) => {
        const controller = controllerOf(model);
        if (!controller) return undefined;
        return controller.complete(
          model,
          position,
          token,
          context.triggerCharacter,
        ) as unknown as Promise<monaco.languages.CompletionList>;
      },
    });
    monaco.languages.registerHoverProvider(language, {
      provideHover: (model, position, token) => {
        const controller = controllerOf(model);
        if (!controller) return null;
        return controller.hover(model, position, token);
      },
    });
  }
}

/** Markers of one editor's model, plus a dotted underline over each hint's whole range. */
function markerSink(editor: monaco.editor.IStandaloneCodeEditor): MarkerSink {
  const hints = editor.createDecorationsCollection();
  return {
    set(markers: MarkerLike[]) {
      const model = editor.getModel();
      if (!model || model.isDisposed()) return;
      monaco.editor.setModelMarkers(model, MARKER_OWNER, markers as monaco.editor.IMarkerData[]);
      hints.set(
        markers
          .filter((m) => m.severity === monaco.MarkerSeverity.Hint)
          .map((m) => ({ range: m, options: { inlineClassName: "insight-hint" } })),
      );
    },
    clear() {
      const model = editor.getModel();
      if (model && !model.isDisposed()) monaco.editor.setModelMarkers(model, MARKER_OWNER, []);
      hints.clear();
    },
  };
}

export interface InsightAttachment {
  controller: InsightController;
  format(): Promise<FormatOutcome>;
  dispose(): void;
}

/** Turns insight on for `editor` (its current model). */
export function attachInsight(
  editor: monaco.editor.IStandaloneCodeEditor,
  options: Omit<InsightControllerOptions, "model" | "enums" | "markers"> & {
    /** How each format command ended (keyboard, context menu or toolbar). */
    onFormat?: (outcome: FormatOutcome) => void;
  },
): InsightAttachment {
  registerProviders();
  const model = editor.getModel();
  const key = model?.uri.toString() ?? null;
  const controller = new InsightController({
    ...options,
    model: () => editor.getModel(),
    enums: ENUMS,
    markers: markerSink(editor),
  });
  if (key) controllers.set(key, controller);
  const changes = editor.onDidChangeModelContent(() => controller.scheduleDiagnostics());
  const format = async () => {
    const outcome = await controller.format(editor);
    options.onFormat?.(outcome);
    return outcome;
  };
  const action = editor.addAction({
    id: "oxbase.formatSql",
    label: "Format SQL (selection or document)",
    keybindings: [
      monaco.KeyMod.Shift | monaco.KeyMod.Alt | monaco.KeyCode.KeyF,
      monaco.KeyMod.chord(
        monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyK,
        monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyF,
      ),
    ],
    contextMenuGroupId: "1_modification",
    contextMenuOrder: 1.5,
    run: async () => {
      await format();
    },
  });
  return {
    controller,
    format,
    dispose() {
      changes.dispose();
      action.dispose();
      controller.dispose();
      if (key && controllers.get(key) === controller) controllers.delete(key);
    },
  };
}
