/** {@link SqlDiffView} on Monaco's diff editor. Loaded lazily by `SqlDiffView`. */
import { useEffect, useRef } from "react";
import { applyMonacoTheme, cssToken, monacoThemeName, subscribeTheme } from "../theme-bridge";
import { monaco } from "./monacoSetup";
import type { SqlDiffViewProps } from "./SqlDiffView";

export default function MonacoDiffView({
  original,
  modified,
  language,
  ariaLabel,
  testId,
}: SqlDiffViewProps) {
  const host = useRef<HTMLDivElement>(null);
  const editor = useRef<monaco.editor.IStandaloneDiffEditor | null>(null);
  const models = useRef<{
    original: monaco.editor.ITextModel;
    modified: monaco.editor.ITextModel;
  } | null>(null);
  const initial = useRef({ original, modified, language });

  useEffect(() => {
    const element = host.current;
    if (!element) return;
    applyMonacoTheme(monaco);
    const instance = monaco.editor.createDiffEditor(element, {
      theme: monacoThemeName(),
      automaticLayout: true,
      readOnly: true,
      originalEditable: false,
      renderSideBySide: true,
      minimap: { enabled: false },
      scrollBeyondLastLine: false,
      fontSize: 13,
      fontFamily: cssToken("--font-mono", "monospace"),
      fontLigatures: false,
      lineHeight: 20,
      renderOverviewRuler: false,
      ariaLabel: ariaLabel ?? "DDL differences",
    });
    const pair = {
      original: monaco.editor.createModel(initial.current.original, initial.current.language),
      modified: monaco.editor.createModel(initial.current.modified, initial.current.language),
    };
    instance.setModel(pair);
    editor.current = instance;
    models.current = pair;
    const unsubscribe = subscribeTheme(() => applyMonacoTheme(monaco));
    return () => {
      unsubscribe();
      instance.setModel(null);
      instance.dispose();
      pair.original.dispose();
      pair.modified.dispose();
      editor.current = null;
      models.current = null;
    };
  }, [ariaLabel]);

  useEffect(() => {
    const pair = models.current;
    if (!pair) return;
    if (pair.original.getValue() !== original) pair.original.setValue(original);
    if (pair.modified.getValue() !== modified) pair.modified.setValue(modified);
  }, [original, modified]);

  useEffect(() => {
    const pair = models.current;
    if (!pair) return;
    monaco.editor.setModelLanguage(pair.original, language);
    monaco.editor.setModelLanguage(pair.modified, language);
  }, [language]);

  return <div ref={host} className="sql-editor sql-diff" data-testid={testId ?? "sql-diff"} />;
}
