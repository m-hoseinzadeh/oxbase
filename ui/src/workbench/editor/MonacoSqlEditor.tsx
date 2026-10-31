/** {@link SqlEditor} on Monaco. Loaded lazily by `SqlEditor`. */
import { useEffect, useRef } from "react";
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
}: SqlEditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const editor = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);
  // Commands read the latest callback without re-creating the editor.
  const command = useRef(onCommand);
  command.current = onCommand;
  const initial = useRef({ value: initialValue, language });

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
    });
    editor.current = instance;
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
    };

    const media = window.matchMedia?.("(prefers-color-scheme: dark)");
    const onScheme = () => monaco.editor.setTheme(prefersDark() ? "vs-dark" : "vs");
    media?.addEventListener("change", onScheme);
    return () => {
      media?.removeEventListener("change", onScheme);
      editorRef.current = null;
      editor.current = null;
      instance.getModel()?.dispose();
      instance.dispose();
    };
  }, [editorRef, ariaLabel]);

  // The connection's engine can change the grammar without recreating the editor.
  useEffect(() => {
    const model = editor.current?.getModel();
    if (model) monaco.editor.setModelLanguage(model, language);
  }, [language]);

  return <div ref={host} className="sql-editor" data-testid="sql-editor" />;
}
