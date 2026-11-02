/** {@link SqlEditor} on Monaco. Loaded lazily by `SqlEditor`. */
import { useEffect, useRef } from "react";
import { hasInsight } from "./insight/controller";
import { attachInsight, type InsightAttachment } from "./insight/monacoInsight";
import { monaco } from "./monacoSetup";
import type { SqlEditorProps } from "./SqlEditor";

function prefersDark(): boolean {
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? false;
}

export default function MonacoSqlEditor({
  initialValue,
  language,
  editorRef,
  onCommand,
  ariaLabel,
  readOnly,
  value,
  testId,
  insight,
}: SqlEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const editor = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);
  // Commands read the latest callback without re-creating the editor.
  const command = useRef(onCommand);
  command.current = onCommand;
  const initial = useRef({ value: value ?? initialValue, language, readOnly: readOnly ?? false });
  // Insight reads the latest target (engine, connection, database) on every request.
  const insightProps = useRef(insight);
  insightProps.current = insight;
  const insightEditorId = insight?.editorId ?? null;
  const attachment = useRef<InsightAttachment | null>(null);

  useEffect(() => {
    const element = host.current;
    if (!element) return;
    const instance = monaco.editor.create(element, {
      value: initial.current.value,
      language: initial.current.language,
      theme: prefersDark() ? "vs-dark" : "vs",
      automaticLayout: true,
      minimap: { enabled: false },
      scrollBeyondLastLine: false,
      fixedOverflowWidgets: true,
      fontSize: 13,
      tabSize: 2,
      renderLineHighlight: "line",
      ariaLabel: ariaLabel ?? "SQL editor",
      readOnly: initial.current.readOnly,
      domReadOnly: initial.current.readOnly,
      // With insight the core offers the names; plain words of the text would only add noise.
      ...(insightEditorId
        ? {
            wordBasedSuggestions: "off" as const,
            quickSuggestions: {
              other: "on" as const,
              comments: "off" as const,
              strings: "off" as const,
            },
            suggest: { showWords: false, filterGraceful: true },
          }
        : {}),
    });
    editor.current = instance;
    if (insightEditorId !== null) {
      attachment.current = attachInsight(instance, {
        editorId: insightEditorId,
        target: () => {
          const current = insightProps.current;
          if (!current || !hasInsight(current.engine)) return null;
          return {
            engine: current.engine,
            sessionId: current.sessionId,
            database: current.database,
          };
        },
        onCacheState: (state) => insightProps.current?.onCacheState?.(state),
        onFormat: (outcome) => insightProps.current?.onFormat?.(outcome),
      });
    }
    instance.addAction({
      id: "oxbase.runCurrent",
      label: "Run selection or current statement",
      keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter],
      run: () => command.current?.("runCurrent"),
    });
    instance.addAction({
      id: "oxbase.runAll",
      label: "Run all",
      keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.Enter],
      run: () => command.current?.("runAll"),
    });
    editorRef.current = {
      getText: () => instance.getValue(),
      getSelection: () => {
        const selection = instance.getSelection();
        const model = instance.getModel();
        if (!selection || !model || selection.isEmpty()) return null;
        return {
          text: model.getValueInRange(selection),
          start: model.getOffsetAt(selection.getStartPosition()),
        };
      },
      getCursorOffset: () => {
        const position = instance.getPosition();
        const model = instance.getModel();
        return position && model ? model.getOffsetAt(position) : 0;
      },
      setText: (text) => instance.setValue(text),
      select: (start, end) => {
        const model = instance.getModel();
        if (!model) return;
        const a = model.getPositionAt(start);
        const b = model.getPositionAt(end);
        const range = new monaco.Selection(a.lineNumber, a.column, b.lineNumber, b.column);
        instance.setSelection(range);
        instance.revealRangeInCenterIfOutsideViewport(range);
      },
      focus: () => instance.focus(),
      insertSnippet: (body) => {
        instance.focus();
        const snippets = instance.getContribution("snippetController2") as unknown as {
          insert(template: string): void;
        } | null;
        if (snippets) snippets.insert(body);
        else instance.trigger("oxbase", "type", { text: body });
      },
      ...(attachment.current
        ? {
            format: () =>
              attachment.current?.format() ?? Promise.resolve({ status: "unavailable" as const }),
          }
        : {}),
    };

    const media = window.matchMedia?.("(prefers-color-scheme: dark)");
    const onScheme = () => monaco.editor.setTheme(prefersDark() ? "vs-dark" : "vs");
    media?.addEventListener("change", onScheme);
    return () => {
      media?.removeEventListener("change", onScheme);
      attachment.current?.dispose();
      attachment.current = null;
      editorRef.current = null;
      editor.current = null;
      instance.getModel()?.dispose();
      instance.dispose();
    };
  }, [editorRef, ariaLabel, insightEditorId]);

  // A new engine, connection, database or cache generation: diagnose again.
  const engine = insight?.engine ?? null;
  const sessionId = insight?.sessionId ?? null;
  const database = insight?.database ?? null;
  const generation = insight?.generation ?? null;
  useEffect(() => {
    void [engine, sessionId, database, generation];
    attachment.current?.controller.scheduleDiagnostics();
  }, [engine, sessionId, database, generation]);

  // The connection's engine can change the grammar without recreating the editor.
  useEffect(() => {
    const model = editor.current?.getModel();
    if (model) monaco.editor.setModelLanguage(model, language);
  }, [language]);

  // A controlled viewer follows `value`.
  useEffect(() => {
    const instance = editor.current;
    if (value === undefined || !instance) return;
    if (instance.getValue() !== value) instance.setValue(value);
  }, [value]);

  return <div ref={host} className="sql-editor" data-testid={testId ?? "sql-editor"} />;
}
