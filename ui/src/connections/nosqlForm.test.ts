import { describe, expect, it } from "vitest";
import {
  emptyForm,
  formFromProfile,
  hostListProblem,
  profileFromForm,
  validateForm,
  withEngine,
} from "./form";
import type { ConnectionProfile } from "./types";

function form(engine: "mongodb" | "redis") {
  return { ...withEngine(emptyForm(), engine), name: "nosql" };
}

describe("MongoDB connection form", () => {
  it("defaults the port and writes only the options that differ from the defaults", () => {
    const f = form("mongodb");
    expect(f.port).toBe("27017");
    const profile = profileFromForm({
      ...f,
      hosts: " db2:27018 , db3 ",
      replicaSet: "rs0",
      authSource: "admin",
      authMechanism: "SCRAM-SHA-256",
      readPreference: "secondaryPreferred",
    });
    expect(profile.engine).toBe("mongodb");
    expect(profile.address).toEqual({ kind: "network", host: "localhost", port: 27017 });
    expect(profile.options).toEqual({
      hosts: "db2:27018,db3",
      replica_set: "rs0",
      auth_source: "admin",
      auth_mechanism: "SCRAM-SHA-256",
      read_preference: "secondaryPreferred",
    });
    expect(profileFromForm(f).options).toBeUndefined();
  });

  it("sends an SRV name without a port", () => {
    const profile = profileFromForm({
      ...form("mongodb"),
      host: "cluster0.example.net",
      srv: true,
    });
    expect(profile.address).toEqual({ kind: "network", host: "cluster0.example.net" });
    expect(profile.options).toEqual({ srv: "true" });
    expect(validateForm({ ...form("mongodb"), srv: true }, "save")).toEqual({});
  });

  it("refuses SRV with SSH or more hosts, more hosts through SSH and PLAIN without verification", () => {
    const ssh = {
      sshEnabled: true,
      sshHost: "bastion",
      sshUser: "me",
      sshPassword: "x",
    };
    expect(validateForm({ ...form("mongodb"), srv: true, ...ssh }, "save").srv).toMatch(
      /SRV name stands alone/,
    );
    expect(validateForm({ ...form("mongodb"), hosts: "db2", ...ssh }, "save").hosts).toMatch(
      /SSH tunnel reaches one server/,
    );
    expect(validateForm({ ...form("mongodb"), hosts: "db2:99999" }, "save").hosts).toMatch(
      /1 to 65535/,
    );
    expect(
      validateForm({ ...form("mongodb"), authMechanism: "PLAIN", tlsMode: "require" }, "save")
        .authMechanism,
    ).toMatch(/PLAIN/);
    expect(
      validateForm({ ...form("mongodb"), authMechanism: "PLAIN", tlsMode: "verify_full" }, "save"),
    ).toEqual({});
  });

  it("reads the options back from a saved profile", () => {
    const saved: ConnectionProfile = {
      schema_version: 1,
      id: "p",
      name: "m",
      engine: "mongodb",
      address: { kind: "network", host: "h", port: 27017 },
      tls: { mode: "prefer" },
      options: { hosts: "a:1", replica_set: "rs", read_preference: "nearest", sample_size: "10" },
    };
    const f = formFromProfile(saved);
    expect(f).toMatchObject({
      engine: "mongodb",
      hosts: "a:1",
      replicaSet: "rs",
      readPreference: "nearest",
    });
    // Options the dialog does not manage are kept.
    expect(profileFromForm(f, saved).options).toEqual({
      hosts: "a:1",
      replica_set: "rs",
      read_preference: "nearest",
      sample_size: "10",
    });
  });
});

describe("Redis connection form", () => {
  it("writes mode, nodes, sentinel and protocol options", () => {
    const profile = profileFromForm({
      ...form("redis"),
      redisMode: "sentinel",
      nodes: "s2:26379",
      masterName: "oxmaster",
      sentinelAuth: "same",
      protocol: "resp2",
      database: "1",
    });
    expect(profile.address).toEqual({ kind: "network", host: "localhost", port: 6379 });
    expect(profile.database).toBe("1");
    expect(profile.options).toEqual({
      mode: "sentinel",
      nodes: "s2:26379",
      master_name: "oxmaster",
      sentinel_auth: "same",
      protocol: "resp2",
    });
  });

  it("keeps an empty namespace separator (no namespaces) and drops the default one", () => {
    expect(profileFromForm({ ...form("redis"), namespaceSeparator: "" }).options).toEqual({
      namespace_separator: "",
    });
    expect(profileFromForm(form("redis")).options).toBeUndefined();
    const back = formFromProfile(profileFromForm({ ...form("redis"), namespaceSeparator: "" }));
    expect(back.namespaceSeparator).toBe("");
    expect(formFromProfile(profileFromForm(form("redis"))).namespaceSeparator).toBe(":");
  });

  it("validates sentinel, cluster and database settings", () => {
    expect(validateForm({ ...form("redis"), redisMode: "sentinel" }, "save").masterName).toMatch(
      /master/,
    );
    expect(
      validateForm({ ...form("redis"), redisMode: "cluster", database: "2" }, "save").database,
    ).toMatch(/only database 0/);
    expect(validateForm({ ...form("redis"), database: "x" }, "save").database).toMatch(/number/);
    expect(
      validateForm(
        { ...form("redis"), redisMode: "cluster", sshEnabled: true, sshHost: "b", sshUser: "u" },
        "save",
      ).redisMode,
    ).toMatch(/standalone/);
  });

  it("checks seed lists", () => {
    expect(hostListProblem("")).toBeNull();
    expect(hostListProblem("a:1, b, [::1]:6379")).toBeNull();
    expect(hostListProblem("a:1,,b")).toMatch(/commas/);
    expect(hostListProblem("a b")).toMatch(/host:port/);
  });
});
