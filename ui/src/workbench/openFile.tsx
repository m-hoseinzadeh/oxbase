/**
 * Ctrl+O: opens a `.sql` file in a new query tab. The native dialog is shown by Rust and the
 * file read there (`file_read_text`, UTF-8, at most 16 MB); the webview never sees a path.
 * A file that cannot be opened says why in a small dialog.
 */
import { useEffect, useState } from "react";
import { filePickOpen, fileReadText, fileRelease } from "../api";
import { Modal } from "../components/Modal";
import { toCommandError } from "../connections/api";
import { openQueryEditor } from "./layout";

const failures = new Set<(message: { name: string; text: string }) => void>();

/** Picks a `.sql` file and opens it in a new query tab titled with its name. */
export async function openSqlFile(): Promise<void> {
  let picked: Awaited<ReturnType<typeof filePickOpen>>;
  try {
    picked = await filePickOpen([{ name: "SQL", extensions: ["sql", "txt"] }], "Open SQL file");
  } catch (error) {
    for (const f of failures) f({ name: "", text: toCommandError(error).message });
    return;
  }
  if (!picked) return;
  try {
    const sql = await fileReadText(picked.handle);
    openQueryEditor({ sql, title: picked.name });
  } catch (error) {
    for (const f of failures) f({ name: picked.name, text: toCommandError(error).message });
  } finally {
    void fileRelease(picked.handle).catch(() => {});
  }
}

/** Shows why a file could not be opened. */
export function OpenFileHost() {
  const [failure, setFailure] = useState<{ name: string; text: string } | null>(null);
  useEffect(() => {
    failures.add(setFailure);
    return () => {
      failures.delete(setFailure);
    };
  }, []);
  if (!failure) return null;
  return (
    <Modal
      title={failure.name ? `Could not open ${failure.name}` : "Could not open the file"}
      onClose={() => setFailure(null)}
      footer={
        <button type="button" className="button button--primary" onClick={() => setFailure(null)}>
          Close
        </button>
      }
    >
      <p>{failure.text}</p>
    </Modal>
  );
}
