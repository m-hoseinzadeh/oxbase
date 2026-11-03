#!/usr/bin/env node
// QA pass: every live (Docker-backed, `#[ignore]`d) test of every engine, one database at a
// time, with a cleanup check between databases. Runs in the QA workflow (.github/workflows/
// qa.yml); it needs Docker and a Rust toolchain, so do not run it on a small dev machine.
//
//   node scripts/qa-engines.mjs [--stages a,b,...] [--out <dir>] [--list-only]
//                               [--test-timeout <min>]
//
// What it does:
//   1. Builds the live test binaries once (`cargo test --no-run`) and lists their ignored tests.
//   2. Puts every test into the stage of the database(s) it needs, from its name (postgres_,
//      mysql::, mongo_shell_..., the crate it lives in). A test that fits no stage fails the run,
//      so a new live test cannot be skipped silently. Tests that belong to other CI jobs (OS
//      keychain, the 1 GB import perf) are listed as excluded with the reason.
//   3. Stages run one after another. Each stage: check that nothing of the previous one is left
//      (containers, volumes, networks, fixture ports, SQLite files), start only that stage's
//      fixtures, snapshot the server's objects, run each test on its own (all CI REQUIRE
//      switches on, OXBASE_CONFORMANCE_ENGINES = the stage's engines), snapshot again (objects
//      the tests left behind or dropped are reported), tear down with volumes, check clean.
//      Single-database stages come first (postgres, mysql, mariadb, sqlite, mssql, mongodb,
//      redis). Features that need two databases by nature (data transfer between engines, the
//      M8 transfer matrix) run last, in stages of exactly the engines they need.
//   4. Writes <out>/qa-results.json, <out>/qa-summary.md (also to $GITHUB_STEP_SUMMARY) and
//      one log per test under <out>/logs/<stage>/.
//
// Exit code 1 when a test failed or timed out, a stage could not start, a cleanup check
// failed, or a test fitted no stage.
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

// --- options -------------------------------------------------------------------------------
const args = process.argv.slice(2);
const opts = { stages: null, out: path.join(repo, "target", "qa"), listOnly: false, timeoutMin: 20 };
for (let i = 0; i < args.length; i++) {
  const a = args[i];
  const next = () => {
    if (i + 1 >= args.length) die(`${a} needs a value`);
    return args[++i];
  };
  if (a === "--stages") opts.stages = next().split(",").map((s) => s.trim()).filter(Boolean);
  else if (a === "--out") opts.out = path.resolve(next());
  else if (a === "--list-only") opts.listOnly = true;
  else if (a === "--test-timeout") opts.timeoutMin = Number(next());
  else if (a === "-h" || a === "--help") {
    console.log("usage: node scripts/qa-engines.mjs [--stages a,b] [--out dir] [--list-only] [--test-timeout min]");
    process.exit(0);
  } else die(`unknown argument ${a}`);
}

function die(msg) {
  console.error(`qa: ${msg}`);
  process.exit(2);
}

// --- engines, stages and fixtures ----------------------------------------------------------
const ENGINES = ["postgres", "mysql", "mariadb", "sqlite", "mssql", "mongodb", "redis"];
const SQL_ENGINES = ["postgres", "mysql", "mariadb", "sqlite", "mssql"];
// Compose services of a single-database stage (the SSH bastion for the tunnel tests).
const STAGE_SERVICES = {
  postgres: ["postgres", "ssh"],
  mysql: ["mysql", "ssh"],
  mariadb: ["mariadb", "ssh"],
  sqlite: [],
  mssql: ["mssql", "ssh"],
  mongodb: ["mongodb", "ssh"],
  redis: ["redis", "redis-cluster", "redis-sentinel", "ssh"],
};
// The packages with live tests (the conformance job's list).
const PACKAGES = [
  "oxbase-conformance", "oxbase-net", "oxbase-pg", "oxbase-mysql", "oxbase-sqlite",
  "oxbase-mssql", "oxbase-mongo", "oxbase-redis", "oxbase-services", "oxbase-io",
  "oxbase-transfer", "oxbase-diff", "oxbase-backup",
];
// The engine of a package whose tests do not name one.
const PACKAGE_ENGINE = {
  "oxbase-pg": "postgres", "oxbase-sqlite": "sqlite", "oxbase-mssql": "mssql",
  "oxbase-mongo": "mongodb", "oxbase-redis": "redis", "oxbase-net": "postgres",
};
// Tests whose name names no engine: `<package>/<target>::<test>` -> engines.
const OVERRIDES = {
  "oxbase-services/m8_transfer_live::continue_mode_skips_and_reports_a_bad_row": ["postgres"],
  "oxbase-services/m8_transfer_live::a_cancel_mid_table_ends_within_two_seconds": ["postgres"],
  "oxbase-services/m8_transfer_live::closing_the_source_session_cancels_the_transfer": ["postgres"],
};
// Tests that run in several stages, scoped by OXBASE_CONFORMANCE_ENGINES.
const HARNESS = "oxbase-conformance/live_fixtures::harness_runs_against_all_engines_and_m1_connection_checks_pass";
const M8_CELLS = "oxbase-conformance/m8_live::m8_sync_transfer_and_backup_cells";
// Live tests of other CI jobs.
const EXCLUDED = [
  [/keychain/, "OS keychain: the keychain job runs it on all three OSes"],
  [/import_perf::import_1gb_csv$/, "1 GB CSV import perf: the import-perf job runs it (perf profile)"],
];

// The same switches as the conformance job of ci.yml: a missing cell, seed or fixture fails.
const REQUIRE_ENV = {
  OXBASE_CONFORMANCE_REQUIRE_M2_FETCH: "1",
  OXBASE_CONFORMANCE_REQUIRE_M3_ROUNDTRIP: "1",
  OXBASE_CONFORMANCE_REQUIRE_M4: "1",
  OXBASE_CONFORMANCE_REQUIRE_M7: "1",
  OXBASE_CONFORMANCE_REQUIRE_M8: "1",
  OXBASE_SERVICES_REQUIRE_M3_CELLS: "1",
  OXBASE_SERVICES_REQUIRE_M5_IO: "1",
  OXBASE_SERVICES_REQUIRE_M7: "1",
  OXBASE_REDIS_REQUIRE_M7: "1",
  OXBASE_SHELL_REQUIRE_M7_SEED: "1",
  OXBASE_SERVICES_REQUIRE_M8: "1",
  RUST_BACKTRACE: "short",
};

// Feature rows of the summary matrix, first match wins (on `<package>/<target>::<test>`).
const FEATURES = [
  ["Conformance matrix (connect, tree, round-trip)", /live_fixtures::harness_runs/],
  ["Conformance M8 cells (sync, transfer, backup)", /m8_live::m8_sync/],
  ["Fixture and seed checks", /fixture_reachable|seed_scripts|seeded_(keys|collections)_answer/],
  ["Data transfer", /m8_transfer_live|oxbase-transfer\//],
  ["Structure sync", /m8_structure_live|oxbase-diff\/.*structure/],
  ["Data sync", /m8_data_live|oxbase-diff\//],
  ["Backup / restore", /m8_backup_live|oxbase-backup\//],
  ["Import / export", /bulk_live|import::|export::|write_sql_live|exports_to_csv|loader_creates/],
  ["Code insight (schema cache)", /schema_cache/],
  ["Grid editing and table designer", /m3_live|dialect_live|oxbase-sqlite\/m3::|edit::|designer::|grid_writer|designer_kinds|alter_column|dml_|apply_edits|documents_insert/],
  ["Object tree and catalog", /catalog_live|fetch_live|tree_|the_tree|lists_|loads_|namespace|acl_users_list/],
  ["Mongo shell / Redis console", /shell_live|console_live|m7_shell_live/],
  ["Documents (find, aggregate, grid)", /documents_live|m7_mongo_live/],
  ["Redis keys and typed editors", /keys_live|m7_redis_live/],
  ["Redis pub/sub", /pubsub_live/],
  ["Redis cluster and sentinel", /cluster_live|sentinel/],
  ["Query editor and results", /query::|scripts_|streams_|results|batches_|m7_big|200k/],
  ["SQL Server specifics (M4)", /oxbase-mssql\/m4_live/],
  ["Connections (plain, TLS, SSH, cancel, auth)", /.*/],
];

// --- helpers -------------------------------------------------------------------------------
function log(msg) {
  console.log(msg);
}

function sh(cmd, argv, { cwd = repo, env = {}, input, quiet = false } = {}) {
  const r = spawnSync(cmd, argv, {
    cwd,
    env: { ...process.env, ...env },
    input,
    encoding: "utf8",
    maxBuffer: 512 * 1024 * 1024,
  });
  if (!quiet && r.status !== 0) {
    process.stderr.write(r.stdout || "");
    process.stderr.write(r.stderr || "");
  }
  return { ok: r.status === 0, out: r.stdout || "", err: r.stderr || "", status: r.status };
}

function fmtMs(ms) {
  if (ms < 1000) return `${ms} ms`;
  if (ms < 120_000) return `${(ms / 1000).toFixed(1)} s`;
  return `${(ms / 60_000).toFixed(1)} min`;
}

function words(s) {
  return s.toLowerCase().split(/[^a-z0-9]+/).filter(Boolean);
}

/// Engines named by `s` (a test path or a target name).
function enginesIn(s) {
  const w = words(s);
  const found = new Set();
  for (let i = 0; i < w.length; i++) {
    const t = w[i];
    if (t === "postgres" || t === "pg" || t === "postgresql") found.add("postgres");
    else if (t === "mysql") found.add("mysql");
    else if (t === "mariadb") found.add("mariadb");
    else if (t === "sqlite") found.add("sqlite");
    else if (t === "mssql" || (t === "sql" && w[i + 1] === "server")) found.add("mssql");
    else if (t === "mongo" || t === "mongodb") found.add("mongodb");
    else if (t === "redis") found.add("redis");
  }
  return ENGINES.filter((e) => found.has(e));
}

// --- 1. build and list ---------------------------------------------------------------------
function buildAndList() {
  log("qa: building the live test binaries");
  const pk = PACKAGES.flatMap((p) => ["-p", p]);
  const r = sh("cargo", ["test", ...pk, "--locked", "--no-run", "--message-format=json-render-diagnostics"], { quiet: true });
  if (!r.ok) {
    process.stderr.write(r.err);
    die("cargo test --no-run failed");
  }
  const bins = [];
  for (const line of r.out.split("\n")) {
    if (!line.startsWith("{")) continue;
    let m;
    try {
      m = JSON.parse(line);
    } catch {
      continue;
    }
    if (m.reason !== "compiler-artifact" || !m.profile?.test || !m.executable) continue;
    const dir = path.dirname(m.manifest_path);
    const pkg = path.basename(dir);
    if (!PACKAGES.includes(pkg)) continue;
    const kind = m.target.kind[0];
    bins.push({ pkg, target: kind === "lib" ? "lib" : m.target.name, exe: m.executable, dir });
  }
  const tests = [];
  for (const b of bins) {
    const l = sh(b.exe, ["--list", "--ignored"], { cwd: b.dir, env: { CARGO_MANIFEST_DIR: b.dir } });
    if (!l.ok) die(`cannot list ${b.exe}`);
    for (const line of l.out.split("\n")) {
      const m = /^(.+): test$/.exec(line.trim());
      if (m) tests.push({ ...b, name: m[1], id: `${b.pkg}/${b.target}::${m[1]}` });
    }
  }
  return tests;
}

// --- 2. classify ---------------------------------------------------------------------------
function classify(tests) {
  const assigned = []; // { test, engines }
  const special = { harness: null, m8: null };
  const excluded = [];
  const unclassified = [];
  for (const t of tests) {
    const ex = EXCLUDED.find(([re]) => re.test(t.id));
    if (ex) {
      excluded.push({ id: t.id, reason: ex[1] });
      continue;
    }
    if (t.id === HARNESS) {
      special.harness = t;
      continue;
    }
    if (t.id === M8_CELLS) {
      special.m8 = t;
      continue;
    }
    let engines = OVERRIDES[t.id];
    if (!engines) engines = enginesIn(t.name);
    if (engines.length === 0) engines = enginesIn(t.target === "lib" ? "" : t.target);
    if (engines.length === 0 && PACKAGE_ENGINE[t.pkg]) engines = [PACKAGE_ENGINE[t.pkg]];
    if (engines.length === 0) unclassified.push(t.id);
    else assigned.push({ test: t, engines });
  }
  return { assigned, special, excluded, unclassified };
}

function planStages({ assigned, special }) {
  const stages = new Map();
  const stage = (engines) => {
    const key = engines.join("+");
    if (!stages.has(key)) stages.set(key, { key, engines, runs: [] });
    return stages.get(key);
  };
  for (const e of ENGINES) stage([e]);
  // The M8 transfer matrix: every pair of SQL engines, both directions, in a stage of its own.
  for (let i = 0; i < SQL_ENGINES.length; i++)
    for (let j = i + 1; j < SQL_ENGINES.length; j++) stage([SQL_ENGINES[i], SQL_ENGINES[j]]);
  for (const a of assigned) stage(a.engines).runs.push(a.test);
  for (const s of stages.values()) {
    if (special.harness && s.engines.length === 1) s.runs.unshift(special.harness);
    if (special.m8 && s.engines.every((e) => SQL_ENGINES.includes(e)) && s.engines.length <= 2)
      s.runs.push(special.m8);
  }
  // Single databases first (ENGINES order), then the cross-engine stages.
  return [...stages.values()].sort((a, b) => {
    if (a.engines.length !== b.engines.length) return a.engines.length - b.engines.length;
    for (let i = 0; i < a.engines.length; i++) {
      const d = ENGINES.indexOf(a.engines[i]) - ENGINES.indexOf(b.engines[i]);
      if (d) return d;
    }
    return 0;
  });
}

// --- 3. fixtures, snapshots and cleanup checks ---------------------------------------------
const PW = "Oxbase_Fixture_Pw_2026";
const SQLITE_FIXTURE = path.join("target", "fixtures", "oxbase_test.sqlite");
const PROJECT = "oxbase-fixtures";
const FIXTURE_PORTS = /:(15[4-5]\d\d)\b/;

function servicesOf(engines) {
  if (engines.length === 1) return STAGE_SERVICES[engines[0]];
  // Cross-engine stages need the databases only (no tunnel tests there).
  return engines.flatMap((e) => STAGE_SERVICES[e].filter((s) => s !== "ssh"));
}

function dockerExec(container, argv, env = {}) {
  const e = Object.entries(env).flatMap(([k, v]) => ["-e", `${k}=${v}`]);
  return sh("docker", ["exec", ...e, container, ...argv], { quiet: true });
}

// Every user-visible object of the engine's server, one line each, sorted.
function snapshot(engine) {
  let r;
  switch (engine) {
    case "postgres":
      r = dockerExec("oxbase-fixtures-postgres", ["psql", "-h", "127.0.0.1", "-U", "oxbase", "-d", "oxbase_test", "-Atc", `
        SELECT 'db ' || datname FROM pg_database WHERE NOT datistemplate
        UNION ALL SELECT 'role ' || rolname FROM pg_roles WHERE rolname NOT LIKE 'pg\\_%'
        UNION ALL SELECT 'schema ' || nspname FROM pg_namespace
          WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema'
        UNION ALL SELECT 'rel ' || n.nspname || '.' || c.relname || ' (' || c.relkind::text || ')'
          FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
            AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
        UNION ALL SELECT 'func ' || n.nspname || '.' || p.proname
          FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
        UNION ALL SELECT 'type ' || n.nspname || '.' || t.typname
          FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
          WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
            AND t.typtype IN ('e', 'd', 'c') AND t.typrelid = 0
        ORDER BY 1`], { PGPASSWORD: PW });
      break;
    case "mysql":
    case "mariadb":
      r = dockerExec(`oxbase-fixtures-${engine}`, [engine === "mysql" ? "mysql" : "mariadb", "-uroot", "-h127.0.0.1", "-N", "-B", "-e", `
        SELECT CONCAT('db ', schema_name) FROM information_schema.schemata
          WHERE schema_name NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys')
        UNION ALL SELECT CONCAT('rel ', table_schema, '.', table_name, ' (', table_type, ')')
          FROM information_schema.tables
          WHERE table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys')
        UNION ALL SELECT CONCAT('routine ', routine_schema, '.', routine_name)
          FROM information_schema.routines WHERE routine_schema NOT IN ('mysql', 'sys')
        UNION ALL SELECT CONCAT('trigger ', trigger_schema, '.', trigger_name)
          FROM information_schema.triggers WHERE trigger_schema NOT IN ('mysql', 'sys')
        UNION ALL SELECT CONCAT('user ', user, '@', host) FROM mysql.user
        ORDER BY 1`], { MYSQL_PWD: PW });
      break;
    case "mssql":
      // Every user database, not only oxbase_test (m8_restore, scratch databases, ...).
      r = dockerExec("oxbase-fixtures-mssql", ["/opt/mssql-tools18/bin/sqlcmd", "-C", "-S", "127.0.0.1", "-U", "sa", "-P", PW, "-h", "-1", "-W", "-Q", `
        SET NOCOUNT ON;
        CREATE TABLE #o (n nvarchar(1000) COLLATE DATABASE_DEFAULT);
        INSERT #o SELECT N'db ' + name FROM sys.databases;
        INSERT #o SELECT N'login ' + name FROM sys.server_principals WHERE type IN ('S', 'U');
        DECLARE @d sysname, @e nvarchar(300), @q nvarchar(max);
        DECLARE c CURSOR LOCAL FAST_FORWARD FOR
          SELECT name FROM sys.databases WHERE database_id > 4 AND state = 0;
        OPEN c;
        FETCH NEXT FROM c INTO @d;
        WHILE @@FETCH_STATUS = 0
        BEGIN
          SET @e = REPLACE(@d, , '');
          SET @q =
            N'INSERT #o SELECT N''obj ' + @e + N'.'' + s.name COLLATE DATABASE_DEFAULT + N''.'' + o.name COLLATE DATABASE_DEFAULT + N'' ('' + RTRIM(o.type) + N'')'' FROM '
              + QUOTENAME(@d) + N'.sys.objects o JOIN ' + QUOTENAME(@d) + N'.sys.schemas s ON s.schema_id = o.schema_id WHERE o.is_ms_shipped = 0 AND o.parent_object_id = 0; '
            + N'INSERT #o SELECT N''schema ' + @e + N'.'' + name COLLATE DATABASE_DEFAULT FROM ' + QUOTENAME(@d) + N'.sys.schemas WHERE schema_id BETWEEN 5 AND 16383; '
            + N'INSERT #o SELECT N''type ' + @e + N'.'' + name COLLATE DATABASE_DEFAULT FROM ' + QUOTENAME(@d) + N'.sys.types WHERE is_user_defined = 1; '
            + N'INSERT #o SELECT N''user ' + @e + N'.'' + name COLLATE DATABASE_DEFAULT FROM ' + QUOTENAME(@d) + N'.sys.database_principals WHERE type IN (''S'', ''U'', ''R'') AND principal_id BETWEEN 5 AND 16383;';
          EXEC (@q);
          FETCH NEXT FROM c INTO @d;
        END
        SELECT n FROM #o ORDER BY n;`]);
      break;
    case "mongodb":
      r = dockerExec("oxbase-fixtures-mongodb", ["mongosh", "--quiet", "--host", "127.0.0.1", "-u", "oxbase", "-p", PW, "--authenticationDatabase", "admin", "--eval", `
        const out = [];
        for (const d of db.adminCommand({ listDatabases: 1 }).databases) {
          out.push('db ' + d.name);
          for (const c of db.getSiblingDB(d.name).getCollectionInfos({}, { nameOnly: true }))
            if (!c.name.startsWith('system.')) out.push('coll ' + d.name + '.' + c.name);
        }
        for (const u of db.getSiblingDB('admin').system.users.find({}, { user: 1, db: 1 }).toArray())
          out.push('user ' + u.db + '.' + u.user);
        print(out.sort().join('\\n'));`]);
      break;
    case "redis": {
      const lines = [];
      const cli = (...a) => dockerExec("oxbase-fixtures-redis", ["redis-cli", "--no-auth-warning", "-a", PW, ...a]);
      const users = cli("ACL", "USERS");
      if (!users.ok) return { ok: false, lines: [], err: users.err };
      for (const u of users.out.split("\n").filter(Boolean)) lines.push(`acl ${u}`);
      for (let db = 0; db < 16; db++) {
        const keys = cli("-n", String(db), "--scan", "--count", "1000");
        if (!keys.ok) return { ok: false, lines: [], err: keys.err };
        for (const k of keys.out.split("\n").filter(Boolean)) lines.push(`key ${db}/${k}`);
      }
      return { ok: true, lines: [...new Set(lines)].sort() };
    }
    case "sqlite":
      // The harness's own fixture file is expected; any other SQLite file under target/ is not.
      return { ok: true, lines: sqliteFiles().filter((f) => f !== SQLITE_FIXTURE).map((f) => `file ${f}`) };
    default:
      return { ok: false, lines: [], err: `no snapshot for ${engine}` };
  }
  return {
    ok: r.ok,
    lines: [...new Set(r.out.split("\n").map((s) => s.trim()).filter(Boolean))].sort(),
    err: r.ok ? "" : (r.err || r.out).trim().slice(0, 500),
  };
}

// SQLite files the tests and the harness create (fixtures, scratch copies), under target/.
function sqliteFiles() {
  const found = [];
  const walk = (dir, depth) => {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) {
        if (depth < 3 && !["debug", "release", "perf", "qa", "tls", "ssh"].includes(e.name)) walk(p, depth + 1);
      } else if (/\.(sqlite|sqlite3|db)(-wal|-shm|-journal)?$/.test(e.name)) {
        found.push(path.relative(repo, p));
      }
    }
  };
  walk(path.join(repo, "target"), 0);
  return found.sort();
}

// What of the fixtures is still there: [] when clean.
function leftovers() {
  const out = [];
  const lines = (r) => r.out.split("\n").map((s) => s.trim()).filter(Boolean);
  const label = `label=com.docker.compose.project=${PROJECT}`;
  for (const c of lines(sh("docker", ["ps", "-a", "--filter", label, "--format", "{{.Names}} ({{.Status}})"], { quiet: true })))
    out.push(`container ${c}`);
  for (const v of lines(sh("docker", ["volume", "ls", "-q", "--filter", label], { quiet: true }))) out.push(`volume ${v}`);
  for (const v of lines(sh("docker", ["volume", "ls", "-q", "--filter", `name=${PROJECT}`], { quiet: true })))
    if (!out.includes(`volume ${v}`)) out.push(`volume ${v}`);
  for (const n of lines(sh("docker", ["network", "ls", "-q", "--filter", label], { quiet: true }))) out.push(`network ${n}`);
  for (const l of lines(sh("ss", ["-Hltn"], { quiet: true }))) {
    const m = FIXTURE_PORTS.exec(l);
    if (m) out.push(`listening port ${m[1]}`);
  }
  for (const f of sqliteFiles()) out.push(`sqlite file ${f}`);
  return [...new Set(out)];
}

function teardown() {
  sh("bash", ["scripts/ci-fixtures.sh", "down"], { quiet: true });
  for (const f of sqliteFiles()) fs.rmSync(path.join(repo, f), { force: true });
  // Scratch output of the conformance runs (M8 scratch databases, reports are kept per stage).
  const conf = path.join(repo, "target", "conformance");
  fs.rmSync(path.join(conf, "m8-scratch"), { recursive: true, force: true });
  if (fs.existsSync(conf))
    for (const f of fs.readdirSync(conf).filter((f) => f.endsWith(".json"))) fs.rmSync(path.join(conf, f), { force: true });
}

// --- 4. run one test -----------------------------------------------------------------------
function runTest(t, stage, logDir) {
  return new Promise((resolve) => {
    const started = Date.now();
    const file = path.join(logDir, `${t.pkg}__${t.target}__${t.name.replace(/[^A-Za-z0-9_.-]+/g, "_")}.log`);
    const fd = fs.openSync(file, "w");
    const child = spawn(t.exe, ["--ignored", "--exact", "--test-threads", "1", "--nocapture", t.name], {
      cwd: t.dir,
      env: {
        ...process.env,
        ...REQUIRE_ENV,
        CARGO_MANIFEST_DIR: t.dir,
        CARGO_PKG_NAME: t.pkg,
        OXBASE_CONFORMANCE_ENGINES: stage.engines.join(","),
      },
      stdio: ["ignore", fd, fd],
    });
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, opts.timeoutMin * 60_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      fs.closeSync(fd);
      const ms = Date.now() - started;
      const text = fs.readFileSync(file, "utf8");
      // libtest exits 0 with "running 0 tests" when the name matched nothing: a QA failure.
      const ran = /test result: \w+\. [1-9]\d* passed|test result: FAILED/.test(text);
      let status = timedOut ? "timeout" : code === 0 ? (ran ? "pass" : "not-run") : "fail";
      resolve({ id: t.id, pkg: t.pkg, target: t.target, name: t.name, status, ms, log: path.relative(opts.out, file), tail: status === "pass" ? "" : text.split("\n").slice(-60).join("\n") });
    });
  });
}

// --- main ----------------------------------------------------------------------------------
async function main() {
  fs.mkdirSync(opts.out, { recursive: true });
  const tests = buildAndList();
  const cls = classify(tests);
  let stages = planStages(cls);
  if (opts.stages) stages = stages.filter((s) => opts.stages.includes(s.key));
  log(`qa: ${tests.length} live tests in ${new Set(tests.map((t) => t.exe)).size} binaries; ${cls.excluded.length} excluded, ${cls.unclassified.length} unclassified`);
  for (const s of stages) log(`  stage ${s.key.padEnd(18)} ${String(s.runs.length).padStart(4)} tests  fixtures: ${servicesOf(s.engines).join(" ") || "(none)"}`);
  for (const u of cls.unclassified) log(`  UNCLASSIFIED ${u}`);
  const results = { generated: new Date().toISOString(), commit: sh("git", ["rev-parse", "HEAD"], { quiet: true }).out.trim(), excluded: cls.excluded, unclassified: cls.unclassified, stages: [] };
  if (opts.listOnly) {
    results.plan = stages.map((s) => ({ stage: s.key, tests: s.runs.map((t) => t.id) }));
    fs.writeFileSync(path.join(opts.out, "qa-plan.json"), JSON.stringify(results, null, 2));
    return cls.unclassified.length ? 1 : 0;
  }

  // Start from nothing.
  teardown();

  for (const s of stages) {
    const st = { stage: s.key, engines: s.engines, started: new Date().toISOString(), tests: [], snapshot: {}, problems: [], cleanBefore: [], cleanAfter: [] };
    results.stages.push(st);
    log(`\n=== stage ${s.key} (${s.runs.length} tests) ===`);
    const t0 = Date.now();
    st.cleanBefore = leftovers();
    if (st.cleanBefore.length) {
      st.problems.push(`not clean before the stage: ${st.cleanBefore.join(", ")}`);
      teardown();
    }
    const services = servicesOf(s.engines);
    if (services.length) {
      const up = sh("bash", ["scripts/ci-fixtures.sh", "up", ...services]);
      if (!up.ok) {
        st.problems.push(`fixtures did not start: ${services.join(" ")}`);
        const logs = sh("bash", ["scripts/ci-fixtures.sh", "logs", ...services], { quiet: true });
        fs.writeFileSync(path.join(opts.out, `fixtures-${s.key}.log`), logs.out + logs.err);
        for (const t of s.runs) st.tests.push({ id: t.id, pkg: t.pkg, target: t.target, name: t.name, status: "not-run", ms: 0, tail: "fixtures did not start" });
        teardown();
        st.cleanAfter = leftovers();
        st.ms = Date.now() - t0;
        continue;
      }
    }
    const before = Object.fromEntries(s.engines.map((e) => [e, snapshot(e)]));
    const logDir = path.join(opts.out, "logs", s.key);
    fs.mkdirSync(logDir, { recursive: true });
    for (const t of s.runs) {
      const r = await runTest(t, s, logDir);
      st.tests.push(r);
      log(`${r.status.toUpperCase().padEnd(7)} ${fmtMs(r.ms).padStart(9)}  ${r.id}`);
      if (r.status !== "pass") log(r.tail.split("\n").map((l) => `    | ${l}`).join("\n"));
    }
    // Objects the stage's tests left behind (or removed from the seed).
    for (const e of s.engines) {
      const after = snapshot(e);
      const b = before[e];
      if (!b.ok || !after.ok) {
        st.snapshot[e] = { error: b.err || after.err || "snapshot failed" };
        continue;
      }
      const bs = new Set(b.lines);
      const as = new Set(after.lines);
      st.snapshot[e] = { objects: b.lines.length, leftBehind: after.lines.filter((l) => !bs.has(l)), dropped: b.lines.filter((l) => !as.has(l)) };
    }
    // Keep the conformance reports of the stage.
    const conf = path.join(repo, "target", "conformance");
    if (fs.existsSync(conf)) {
      for (const f of fs.readdirSync(conf).filter((f) => f.endsWith(".json")))
        fs.copyFileSync(path.join(conf, f), path.join(logDir, `conformance-${f}`));
    }
    if (st.tests.some((t) => t.status !== "pass")) {
      const logs = sh("bash", ["scripts/ci-fixtures.sh", "logs", ...services], { quiet: true });
      if (services.length) fs.writeFileSync(path.join(opts.out, `fixtures-${s.key}.log`), logs.out + logs.err);
    }
    teardown();
    st.cleanAfter = leftovers();
    if (st.cleanAfter.length) st.problems.push(`not clean after teardown: ${st.cleanAfter.join(", ")}`);
    st.ms = Date.now() - t0;
    const n = (k) => st.tests.filter((t) => t.status === k).length;
    log(`=== ${s.key}: ${n("pass")} passed, ${n("fail")} failed, ${n("timeout")} timed out, ${n("not-run")} not run; cleanup ${st.cleanAfter.length ? "LEFT " + st.cleanAfter.join(", ") : "clean"} (${fmtMs(st.ms)})`);
    fs.writeFileSync(path.join(opts.out, "qa-results.json"), JSON.stringify(results, null, 2));
  }

  fs.writeFileSync(path.join(opts.out, "qa-results.json"), JSON.stringify(results, null, 2));
  const md = summary(results);
  fs.writeFileSync(path.join(opts.out, "qa-summary.md"), md);
  if (process.env.GITHUB_STEP_SUMMARY) fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, md);
  const bad = results.stages.some((s) => s.problems.length || s.tests.some((t) => t.status !== "pass"));
  return bad || results.unclassified.length ? 1 : 0;
}

function feature(id) {
  return FEATURES.find(([, re]) => re.test(id))[0];
}

function summary(results) {
  const L = [];
  L.push(`# QA: every live test, one database at a time`, "");
  L.push(`Commit \`${results.commit.slice(0, 12)}\`, ${results.generated}.`, "");
  L.push("| Stage | Passed | Failed | Timed out | Not run | Time | Objects left behind | Cleanup |", "|---|---|---|---|---|---|---|---|");
  for (const s of results.stages) {
    const n = (k) => s.tests.filter((t) => t.status === k).length;
    const leaks = Object.entries(s.snapshot).map(([e, v]) => (v.error ? `${e}: snapshot failed` : v.leftBehind.length ? `${e}: ${v.leftBehind.length}` : "")).filter(Boolean).join(", ") || "none";
    L.push(`| ${s.stage} | ${n("pass")} | ${n("fail")} | ${n("timeout")} | ${n("not-run")} | ${fmtMs(s.ms ?? 0)} | ${leaks} | ${s.cleanAfter.length ? "LEFT: " + s.cleanAfter.join(", ") : "clean"} |`);
  }
  // Feature x engine matrix (single-database stages, then "cross" for multi-engine ones).
  const cols = [...ENGINES, "cross-engine"];
  const cell = {};
  for (const s of results.stages) {
    const col = s.engines.length === 1 ? s.engines[0] : "cross-engine";
    for (const t of s.tests) {
      const f = feature(t.id);
      cell[f] ??= {};
      cell[f][col] ??= { pass: 0, all: 0 };
      cell[f][col].all++;
      if (t.status === "pass") cell[f][col].pass++;
    }
  }
  L.push("", "## Features by database (passed / run)", "", `| Feature | ${cols.join(" | ")} |`, `|---|${cols.map(() => "---").join("|")}|`);
  for (const [f] of FEATURES) {
    if (!cell[f]) continue;
    L.push(`| ${f} | ${cols.map((c) => (cell[f][c] ? `${cell[f][c].pass === cell[f][c].all ? "✅" : "❌"} ${cell[f][c].pass}/${cell[f][c].all}` : "–")).join(" | ")} |`);
  }
  const failed = results.stages.flatMap((s) => s.tests.filter((t) => t.status !== "pass").map((t) => ({ ...t, stage: s.stage })));
  if (failed.length) {
    L.push("", "## Failed tests", "");
    for (const t of failed) {
      L.push(`<details><summary><b>${t.status}</b> [${t.stage}] <code>${t.id}</code> (${fmtMs(t.ms)})</summary>`, "", "```", t.tail.split("\n").slice(-40).join("\n"), "```", "</details>", "");
    }
  }
  const leaks = results.stages.flatMap((s) => Object.entries(s.snapshot).filter(([, v]) => !v.error && (v.leftBehind.length || v.dropped.length)).map(([e, v]) => ({ stage: s.stage, e, v })));
  if (leaks.length) {
    L.push("", "## Server objects changed by the tests", "", "Objects present after the stage's tests that were not there before (left behind), and seed objects that were gone afterwards (dropped).", "");
    for (const { stage, e, v } of leaks) {
      const show = (a) => a.slice(0, 30).map((x) => `\`${x}\``).join(", ") + (a.length > 30 ? `, ... (${a.length} in all)` : "");
      if (v.leftBehind.length) L.push(`- **${stage} / ${e}** left behind ${v.leftBehind.length}: ${show(v.leftBehind)}`);
      if (v.dropped.length) L.push(`- **${stage} / ${e}** dropped ${v.dropped.length}: ${show(v.dropped)}`);
    }
  }
  const snapErr = results.stages.flatMap((s) => Object.entries(s.snapshot).filter(([, v]) => v.error).map(([e, v]) => `- ${s.stage} / ${e}: ${v.error}`));
  if (snapErr.length) L.push("", "## Snapshot errors", "", ...snapErr);
  const problems = results.stages.flatMap((s) => s.problems.map((p) => `- ${s.stage}: ${p}`));
  if (problems.length) L.push("", "## Stage problems", "", ...problems);
  if (results.unclassified.length) L.push("", "## Tests that fit no stage", "", ...results.unclassified.map((u) => `- \`${u}\``));
  L.push("", "## Excluded (run by other CI jobs)", "", ...results.excluded.map((x) => `- \`${x.id}\`: ${x.reason}`), "");
  return L.join("\n");
}

main().then((code) => process.exit(code), (err) => {
  console.error(err);
  process.exit(2);
});
