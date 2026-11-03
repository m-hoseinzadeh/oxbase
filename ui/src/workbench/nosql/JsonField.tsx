/**
 * A small Extended JSON editor (Monaco, JavaScript grammar) with a label and a problem line,
 * for the find builder's fields and the pipeline's stages.
 */
import { useId, useRef } from "react";
import { SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";

export interface JsonFieldProps {
  label: string;
  value: string;
  onChange: (text: string) => void;
  placeholder?: string | undefined;
  problem?: string | undefined;
  /** Editor height in lines (it scrolls past them). */
  lines?: number | undefined;
  testId?: string | undefined;
  /** Ctrl+Enter in the field. */
  onRun?: (() => void) | undefined;
}

export function JsonField({
  label,
  value,
  onChange,
  placeholder,
  problem,
  lines = 3,
  testId,
  onRun,
}: JsonFieldProps) {
  const id = useId();
  const editorRef = useRef<SqlEditorHandle | null>(null);
  return (
    <div className={`json-field${problem ? " json-field--invalid" : ""}`}>
      <span className="json-field__label" id={`${id}-label`}>
        {label}
        {placeholder ? <span className="json-field__placeholder">{placeholder}</span> : null}
      </span>
      <div className="json-field__editor" style={{ height: `${lines * 20 + 10}px` }}>
        <SqlEditor
          initialValue={value}
          language="javascript"
          editorRef={editorRef}
          ariaLabel={label}
          onChange={onChange}
          onCommand={() => onRun?.()}
          compact
          testId={testId}
        />
      </div>
      {problem ? (
        <p className="json-field__problem" role="alert">
          {problem}
        </p>
      ) : null}
    </div>
  );
}
