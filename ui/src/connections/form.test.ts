import { describe, expect, it } from "vitest";
import {
  emptyForm,
  firstError,
  formFromProfile,
  profileFromForm,
  promptedSecrets,
  secretsFromForm,
  validateForm,
  withEngine,
} from "./form";
import { type ConnectionProfile, NEW_PROFILE_ID } from "./types";

describe("validateForm", () => {
  it("needs a name to save but not to test", () => {
    const form = emptyForm();
    expect(validateForm(form, "save").name).toBeDefined();
    expect(validateForm(form, "test")).toEqual({});
  });

  it("checks host and port for network engines", () => {
    const form = { ...emptyForm(), name: "x", host: " ", port: "70000" };
    const errors = validateForm(form, "save");
    expect(errors.host).toBeDefined();
    expect(errors.port).toMatch(/1 to 65535/);
    expect(firstError(errors)).toBe("host");
    expect(validateForm({ ...form, host: "db", port: "" }, "save")).toEqual({});
    expect(validateForm({ ...form, host: "db", port: "54a" }, "save").port).toBeDefined();
  });

  it("needs a file for SQLite and ignores network fields", () => {
    const form = { ...withEngine(emptyForm(), "sqlite"), name: "x", host: "" };
    expect(validateForm(form, "save")).toEqual({ filePath: "Choose the SQLite database file." });
    expect(validateForm({ ...form, filePath: "/tmp/a.db" }, "save")).toEqual({});
  });

  it("checks client certificate and key together", () => {
    const form = { ...emptyForm(), name: "x", clientCert: "/c.pem" };
    expect(validateForm(form, "save").clientKey).toBeDefined();
    const keyOnly = { ...form, clientCert: "", clientKey: "/k.pem" };
    expect(validateForm(keyOnly, "save").clientCert).toBeDefined();
  });

  it("checks SSH settings only when the tunnel is on", () => {
    const off = { ...emptyForm(), name: "x", sshPort: "" };
    expect(validateForm(off, "save")).toEqual({});
    const on = {
      ...off,
      sshEnabled: true,
      sshAuth: "private_key" as const,
      hostKeyPolicy: "fingerprint" as const,
      fingerprint: "MD5:aa",
    };
    expect(Object.keys(validateForm(on, "save")).sort()).toEqual([
      "fingerprint",
      "sshHost",
      "sshKeyFile",
      "sshPort",
      "sshUser",
    ]);
  });
});

describe("withEngine", () => {
  it("moves a default port to the new engine's default and keeps a custom one", () => {
    expect(withEngine(emptyForm("postgres"), "mysql").port).toBe("3306");
    expect(withEngine({ ...emptyForm("postgres"), port: "6543" }, "mysql").port).toBe("6543");
    expect(withEngine(emptyForm("mysql"), "sqlite").port).toBe("");
  });
});

describe("profileFromForm", () => {
  it("builds a new network profile with secret references only", () => {
    const form = {
      ...emptyForm(),
      name: " prod ",
      host: "db.example.com",
      port: "5433",
      user: "app",
      password: "hunter2-marker",
      tlsMode: "verify_full" as const,
      caCert: "/etc/ca.pem",
    };
    const profile = profileFromForm(form);
    expect(profile).toEqual({
      schema_version: 1,
      id: NEW_PROFILE_ID,
      name: "prod",
      engine: "postgres",
      address: { kind: "network", host: "db.example.com", port: 5433 },
      user: "app",
      password: { source: "keychain", key: "pending" },
      tls: { mode: "verify_full", ca_cert: { source: "file", path: "/etc/ca.pem" } },
    });
    expect(JSON.stringify(profile)).not.toContain("hunter2-marker");
    expect(secretsFromForm(form)).toEqual({ password: "hunter2-marker" });
  });

  it("asks every time when saving is off", () => {
    const profile = profileFromForm({ ...emptyForm(), name: "x", savePassword: false });
    expect(profile.password).toEqual({ source: "prompt" });
    expect(promptedSecrets(profile)).toEqual(["password"]);
  });

  it("builds SSH settings", () => {
    const form = {
      ...emptyForm(),
      name: "x",
      sshEnabled: true,
      sshHost: "bastion",
      sshUser: "ops",
      sshAuth: "private_key" as const,
      sshKeyFile: "/home/ops/.ssh/id_ed25519",
      sshPassphrase: "pass-marker",
      saveSshSecret: false,
      hostKeyPolicy: "accept_new" as const,
    };
    const profile = profileFromForm(form);
    expect(profile.ssh).toEqual({
      host: "bastion",
      port: 22,
      user: "ops",
      auth: {
        method: "private_key",
        key: { source: "file", path: "/home/ops/.ssh/id_ed25519" },
        passphrase: { source: "prompt" },
      },
      known_hosts: { policy: "accept_new" },
    });
    expect(secretsFromForm(form)).toEqual({ ssh_key_passphrase: "pass-marker" });
    expect(secretsFromForm({ ...form, sshEnabled: false })).toEqual({});
  });

  it("treats a key without a passphrase as unencrypted, also with saving off", () => {
    const form = {
      ...emptyForm(),
      name: "x",
      sshEnabled: true,
      sshHost: "bastion",
      sshUser: "ops",
      sshAuth: "private_key" as const,
      sshKeyFile: "/home/ops/.ssh/id_ed25519",
      sshPassphrase: "",
      saveSshSecret: false,
    };
    const profile = profileFromForm(form);
    expect(profile.ssh?.auth).toEqual({
      method: "private_key",
      key: { source: "file", path: "/home/ops/.ssh/id_ed25519" },
    });
    expect(promptedSecrets(profile)).toEqual([]);
    expect(secretsFromForm(form)).toEqual({});

    // A profile that already asks every time keeps asking when nothing is typed.
    const prompting = profileFromForm({ ...form, sshPassphrase: "typed-once" });
    expect(promptedSecrets(prompting)).toEqual(["ssh_key_passphrase"]);
    const edited = profileFromForm(formFromProfile(prompting), prompting);
    expect(promptedSecrets(edited)).toEqual(["ssh_key_passphrase"]);
  });

  it("builds a SQLite profile without network settings", () => {
    const form = { ...withEngine(emptyForm(), "sqlite"), name: "local", filePath: "/tmp/a.db" };
    const profile = profileFromForm({ ...form, sshEnabled: true, sshHost: "b", sshUser: "u" });
    expect(profile.address).toEqual({ kind: "file", path: "/tmp/a.db" });
    expect(profile.ssh).toBeUndefined();
    expect(profile.password).toBeUndefined();
    expect(profile.tls).toEqual({ mode: "disable" });
  });

  it("round-trips an edited profile and keeps fields it does not know", () => {
    const saved = {
      schema_version: 3,
      id: "0190f000-0000-7000-8000-000000000001",
      name: "staging",
      engine: "mysql",
      address: { kind: "network", host: "10.0.0.5" },
      user: "root",
      password: { source: "keychain", key: "connection/x/password" },
      tls: { mode: "require", pinned: ["abc"] },
      ssh: {
        host: "jump",
        port: 2222,
        user: "me",
        auth: { method: "password", password: { source: "keychain", key: "k" } },
        known_hosts: { policy: "fingerprint", fingerprint: "SHA256:abc" },
        keepalive_secs: 30,
      },
      color: "teal",
    } as unknown as ConnectionProfile;
    const form = formFromProfile(saved);
    expect(form.password).toBe("");
    expect(form.port).toBe("");
    expect(form.sshPort).toBe("2222");
    const again = profileFromForm(form, saved);
    expect(again).toEqual(saved);
  });
});
