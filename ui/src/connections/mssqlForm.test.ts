import { describe, expect, it } from "vitest";
import {
  emptyForm,
  engineTraits,
  firstError,
  formFromProfile,
  profileFromForm,
  validateForm,
  withEngine,
} from "./form";
import { type ConnectionProfile, DIALOG_ENGINES } from "./types";

const mssql = () => ({ ...withEngine(emptyForm(), "mssql"), name: "sql", user: "sa" });

describe("SQL Server connection form", () => {
  it("is offered with port 1433 and its extras", () => {
    expect(DIALOG_ENGINES).toContain("mssql");
    expect(mssql().port).toBe("1433");
    const traits = engineTraits("mssql");
    expect(traits).toMatchObject({
      instance: true,
      strictEncryption: true,
      readOnlyIntent: true,
      clientCertificates: false,
    });
    expect(traits.tlsHelp?.require).toMatch(/TrustServerCertificate/);
    expect(engineTraits("postgres")).toMatchObject({ instance: false, clientCertificates: true });
  });

  it("writes instance, strict encryption and read-only intent as driver options", () => {
    const profile = profileFromForm({
      ...mssql(),
      instance: " SQLEXPRESS ",
      port: "14330",
      tlsMode: "verify_full",
      strictEncryption: true,
      readOnlyIntent: true,
    });
    expect(profile.engine).toBe("mssql");
    expect(profile.address).toEqual({ kind: "network", host: "localhost", port: 14330 });
    expect(profile.options).toEqual({
      instance: "SQLEXPRESS",
      encrypt: "strict",
      application_intent: "read_only",
    });
    const back = formFromProfile(profile);
    expect(back.engine).toBe("mssql");
    expect(back.instance).toBe("SQLEXPRESS");
    expect(back.strictEncryption).toBe(true);
    expect(back.readOnlyIntent).toBe(true);
  });

  it("writes no options when none are set and keeps unknown ones of the same engine", () => {
    expect(profileFromForm(mssql()).options).toBeUndefined();
    const base: ConnectionProfile = {
      ...profileFromForm(mssql()),
      options: { batch_rows: "1000", encrypt: "strict", instance: "OLD" },
    };
    const edited = profileFromForm({ ...formFromProfile(base), instance: "", name: "x" }, base);
    // Turning strict off and clearing the instance removes those keys; others stay.
    expect(edited.options).toEqual({ batch_rows: "1000", encrypt: "strict" });
    const off = profileFromForm(
      { ...formFromProfile(base), strictEncryption: false, instance: "", name: "x" },
      base,
    );
    expect(off.options).toEqual({ batch_rows: "1000" });
  });

  it("drops the options of another engine when the engine changes", () => {
    const pg: ConnectionProfile = {
      ...profileFromForm({ ...emptyForm(), name: "pg" }),
      options: { sslnegotiation: "direct" },
    };
    const moved = profileFromForm(withEngine(formFromProfile(pg), "mssql"), pg);
    expect(moved.options).toBeUndefined();
    const back = profileFromForm({ ...mssql(), instance: "X" }, pg);
    expect(back.options).toEqual({ instance: "X" });
  });

  it("allows strict encryption only with Require or a verifying mode", () => {
    const strict = { ...mssql(), strictEncryption: true };
    for (const mode of ["disable", "prefer"] as const) {
      const errors = validateForm({ ...strict, tlsMode: mode }, "save");
      expect(errors.strictEncryption).toMatch(/Require/);
      expect(firstError(errors)).toBe("strictEncryption");
    }
    for (const mode of ["require", "verify_ca", "verify_full"] as const) {
      expect(validateForm({ ...strict, tlsMode: mode }, "save")).toEqual({});
    }
    // Other engines have no such switch: a stale flag is ignored.
    expect(validateForm({ ...emptyForm(), name: "pg", strictEncryption: true }, "save")).toEqual(
      {},
    );
  });

  it("needs a port for a named instance and only the instance name", () => {
    const errors = validateForm({ ...mssql(), instance: "SQLEXPRESS", port: "" }, "save");
    expect(errors.port).toMatch(/instance's port/);
    expect(validateForm({ ...mssql(), instance: "SQLEXPRESS" }, "save")).toEqual({});
    expect(validateForm({ ...mssql(), instance: "host\\SQLEXPRESS" }, "save").instance).toMatch(
      /only the instance name/,
    );
  });

  it("does not send client certificates (the driver refuses them)", () => {
    const form = { ...mssql(), clientCert: "/c.pem", clientKey: "" };
    expect(validateForm(form, "save")).toEqual({});
    expect(profileFromForm({ ...form, clientKey: "/k.pem" }).tls).toEqual({ mode: "prefer" });
  });
});
