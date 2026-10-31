import { tableFromArrays, tableToIPC } from "apache-arrow";
import { describe, expect, it } from "vitest";
import { decodeArrowPage, toBytes } from "./ipc";

describe("decodeArrowPage", () => {
  const ipc = tableToIPC(tableFromArrays({ id: Int32Array.from([1, 2, 3]) }), "stream");

  it("decodes an ArrayBuffer, a Uint8Array and a number array", () => {
    const asBuffer = ipc.slice().buffer;
    expect(decodeArrowPage(asBuffer).numRows).toBe(3);
    expect(decodeArrowPage(ipc).numRows).toBe(3);
    expect(decodeArrowPage(Array.from(ipc)).numRows).toBe(3);
  });

  it("rejects bytes that are not an Arrow IPC stream", () => {
    expect(() => decodeArrowPage(new Uint8Array([1, 2, 3]))).toThrow(/no schema/);
  });

  it("keeps Uint8Array input without copying", () => {
    expect(toBytes(ipc)).toBe(ipc);
  });
});
