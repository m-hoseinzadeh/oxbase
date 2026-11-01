import { describe, expect, it } from "vitest";
import { languageFor, messagesNoteFor } from "./language";

describe("editor language", () => {
  it("maps engines to Monaco grammars (SQL Server uses the T-SQL `sql` grammar)", () => {
    expect(languageFor("mssql")).toBe("sql");
    expect(languageFor("postgres")).toBe("pgsql");
    expect(languageFor("mariadb")).toBe("mysql");
    expect(languageFor("sqlite")).toBe("sql");
    expect(languageFor(undefined)).toBe("sql");
  });

  it("notes that SQL Server PRINT output is not captured", () => {
    expect(messagesNoteFor("mssql")).toMatch(/PRINT/);
    expect(messagesNoteFor("postgres")).toBeNull();
    expect(messagesNoteFor(undefined)).toBeNull();
  });
});
