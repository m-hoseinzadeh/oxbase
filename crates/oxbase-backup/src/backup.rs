//! Logical backup of a database or schema (`docs/m8-contracts.md` §7.2), by the **backup**
//! stream.
//!
//! # Steps ([`backup`])
//!
//! 1. Read the container through the engine's fetchers (`objects::load_container`: tables,
//!    views, materialized views, sequences, routines, triggers; [`BackupSpec::objects`] narrows
//!    it, a selected table brings its triggers). Deviation from the contract text
//!    (`oxbase_diff::load_snapshot`): a backup needs the definitions one by one in creation
//!    order, not a comparable snapshot, so it reads them itself with the same fetchers.
//! 2. Order (`objects::creation_order`): sequences, routines the tables' expressions call,
//!    tables by foreign keys, other routines, views (dependency order), triggers; recorded as
//!    `Manifest::order`.
//! 3. Per object: its definition JSON and its `CREATE` text (the source engine's
//!    `DdlGenerator::create`, skipped with a warning when the engine has no generator for the
//!    kind).
//! 4. Per table (content `structure_and_data`): `SELECT` of every stored column in primary-key
//!    order when there is one, streamed from a dedicated session, converted to the load schema
//!    (`load::to_load_batch`) and written as Arrow IPC batches. Memory: one batch at a time.
//!    Left out (with a manifest warning): generated columns, and server-maintained columns the
//!    engine refuses values for (`load::server_maintained`). Each table's rows are read in one
//!    transaction of its own at the session's default isolation (`Connection::begin`); tables
//!    are not consistent with each other, and the manifest says so. (A snapshot read across
//!    tables needs an engine-neutral isolation role, which M8 does not have.)
//! 5. Warnings in the manifest for what a restore cannot bring back exactly: time values finer
//!    than microseconds, sequences' positions (a restore restarts each sequence after its
//!    restored values), and, for a backup of selected objects, foreign keys to tables left out.
//! 6. The manifest last; the file is a temporary file next to the destination, renamed on
//!    success (`oxbase_io::SinkFile`). A failed or cancelled backup leaves no file and never
//!    damages an existing one.
//!
//! Progress: [`BackupProgress`] at most every 250 ms and after each object. Cancel: within 2 s
//! (the running `SELECT` is cancelled through the driver, the temporary file deleted).

use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use arrow_schema::{DataType as ArrowType, SchemaRef};
use chrono::Utc;
use futures::StreamExt;
use oxbase_dialect::deps::same_table;
use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{DdlContext, DialectRegistry, LoadColumn, LoadTarget};
use oxbase_driver::{CancellationToken, Connection, ResultItem};
use oxbase_io::SinkFile;
use oxbase_types::{
    DataType, Error, Location, ObjectDef, ObjectKind, ObjectRef, Result, ServerVersion, TableDef,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::archive::ArchiveWriter;
use crate::format::{
    BackupContent, BackupSource, Compression, EntryInfo, FORMAT_NAME, FORMAT_VERSION, Manifest,
    ManifestObject, TableData, data_entry, ddl_entry, definition_entry,
};
use crate::load::{load_columns, to_load_batch};
use crate::objects::{creation_order, load_container};

/// What to back up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSpec {
    /// The database (MySQL, MariaDB, SQLite) or schema (PostgreSQL, SQL Server).
    pub container: ObjectRef,
    /// Objects to include (empty: every object of the container).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<ObjectRef>,
    #[serde(default)]
    pub content: BackupContent,
    #[serde(default)]
    pub compression: Compression,
}

/// Cumulative backup progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupProgress {
    pub objects_done: u32,
    pub objects_total: u32,
    /// The object being written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ObjectRef>,
    pub rows: u64,
    /// Bytes written to the archive so far.
    pub bytes: u64,
    pub elapsed_ms: u64,
}

/// What a finished backup wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupReport {
    pub manifest: Manifest,
    /// The archive's size in bytes.
    pub bytes: u64,
    pub elapsed_ms: u64,
}

/// The sessions and registry a backup reads through.
#[derive(Clone, Copy)]
pub struct BackupJob<'a> {
    pub dialects: &'a DialectRegistry,
    /// Reads the catalog (definitions).
    pub metadata: &'a dyn Connection,
    /// Reads table rows (a dedicated session: it holds the snapshot transaction).
    pub rows: &'a dyn Connection,
    pub spec: &'a BackupSpec,
}

/// Writes the backup of `job.spec` into `destination` (module docs). `overwrite` replaces an
/// existing file (the app asked the user in its save dialog).
pub async fn backup(
    job: BackupJob<'_>,
    destination: &std::path::Path,
    overwrite: bool,
    on_progress: &(dyn Fn(&BackupProgress) + Send + Sync),
    cancel: CancellationToken,
) -> Result<BackupReport> {
    let started = Instant::now();
    let spec = job.spec;
    if !matches!(
        spec.container.kind,
        ObjectKind::Database | ObjectKind::Schema
    ) {
        return Err(Error::invalid_input(format!(
            "a backup is of a database or a schema, not of a {}",
            spec.container.kind
        )));
    }
    let engine = job.metadata.engine();
    if job.dialects.fetcher(engine, ObjectKind::Table).is_err() {
        return Err(Error::unsupported(format!(
            "Oxbase cannot back up {} databases",
            engine.display_name()
        )));
    }
    check(&cancel)?;
    let sink = {
        let destination = destination.to_path_buf();
        tokio::task::spawn_blocking(move || SinkFile::create(&destination, overwrite))
            .await
            .map_err(|_| Error::internal("creating the backup file stopped unexpectedly"))??
    };
    let bytes = Arc::new(AtomicU64::new(0));
    let archive = ArchiveTask::spawn(sink, spec.compression, Arc::clone(&bytes));
    let (manifest, mut progress) = {
        let mut run = Run {
            job,
            archive: &archive,
            bytes: &bytes,
            started,
            on_progress,
            cancel: &cancel,
            progress: BackupProgress::default(),
            last_report: None,
        };
        let manifest = run.write().await;
        (manifest, run.progress)
    };
    let finished = match manifest {
        Ok(manifest) => archive
            .finish(manifest.clone())
            .await
            .map(|size| (manifest, size)),
        Err(e) => Err(e),
    };
    // The writer thread ends (and deletes the temporary file unless committed) before this
    // returns: a failed or cancelled backup leaves no file.
    archive.close().await;
    let (manifest, size) = match finished {
        Ok(done) => done,
        Err(_) if cancel.is_cancelled() => return Err(Error::Cancelled),
        Err(e) => return Err(e),
    };
    progress.bytes = size;
    progress.current = None;
    progress.elapsed_ms = elapsed_ms(started);
    on_progress(&progress);
    Ok(BackupReport {
        manifest,
        bytes: size,
        elapsed_ms: elapsed_ms(started),
    })
}

fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Runs `fut` unless `cancel` fires first.
pub(crate) async fn until_cancelled<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        r = fut => r,
    }
}

/// Progress events at most this often while rows stream.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// One backup run.
struct Run<'a> {
    job: BackupJob<'a>,
    archive: &'a ArchiveTask,
    bytes: &'a AtomicU64,
    started: Instant,
    on_progress: &'a (dyn Fn(&BackupProgress) + Send + Sync),
    cancel: &'a CancellationToken,
    progress: BackupProgress,
    last_report: Option<Instant>,
}

impl Run<'_> {
    fn report(&mut self, force: bool) {
        let now = Instant::now();
        if !force && self.last_report.is_some_and(|t| now - t < PROGRESS_EVERY) {
            return;
        }
        self.last_report = Some(now);
        self.progress.bytes = self.bytes.load(Ordering::Relaxed);
        self.progress.elapsed_ms = elapsed_ms(self.started);
        (self.on_progress)(&self.progress);
    }

    /// Everything but the commit: definitions, DDL, rows; returns the manifest.
    async fn write(&mut self) -> Result<Manifest> {
        let job = self.job;
        let spec = job.spec;
        let engine = job.metadata.engine();
        let version = job.metadata.server_version().clone();
        let loaded = load_container(
            job.dialects,
            job.metadata,
            &spec.container,
            &spec.objects,
            &|_, _| {},
            self.cancel,
        )
        .await?;
        let mut warnings = loaded.warnings;
        let mut objects = creation_order(loaded.objects);
        if !spec.content.has_structure() {
            // Data only: the tables, whose definitions read the rows back; nothing else.
            objects.retain(|d| matches!(d, ObjectDef::Table(_)));
        }
        warnings.extend(fidelity_warnings(&objects, spec));
        self.progress.objects_total = u32::try_from(objects.len()).unwrap_or(u32::MAX);
        self.report(true);

        let quoting = job.dialects.quoting(engine)?;
        let terminator = quoting.script_terminator();
        let mut manifest_objects = Vec::with_capacity(objects.len());
        let mut entries = std::collections::BTreeMap::new();
        let mut without_ddl: Vec<ObjectKind> = Vec::new();
        let with_data = spec.content.has_rows();
        if with_data && objects.iter().any(|d| matches!(d, ObjectDef::Table(_))) {
            warnings.push(
                "each table's rows were read in a transaction of their own; rows of different \
                 tables may come from different moments"
                    .to_owned(),
            );
        }
        for (position, def) in objects.iter().enumerate() {
            check(self.cancel)?;
            let index = u32::try_from(position + 1)
                .map_err(|_| Error::invalid_input("too many objects for one backup"))?;
            let object = def.object_ref();
            self.progress.current = Some(object.clone());
            self.report(true);
            let ddl = if let Ok(generator) = job.dialects.ddl(engine, def.kind()) {
                match generator.create(def, &DdlContext::new(&version)) {
                    Ok(script) => Some(script.to_sql(terminator)),
                    Err(e) => {
                        warnings.push(format!(
                            "no CREATE text for {} {}: {e}",
                            def.kind(),
                            object.display_name()
                        ));
                        None
                    }
                }
            } else {
                if !without_ddl.contains(&def.kind()) {
                    without_ddl.push(def.kind());
                }
                None
            };
            let (def_info, ddl_info) = self
                .archive
                .object(index, def.clone(), ddl.clone(), self.cancel)
                .await?;
            entries.insert(definition_entry(index), def_info);
            if let Some(info) = ddl_info {
                entries.insert(ddl_entry(index), info);
            }
            let data = match def {
                ObjectDef::Table(table) if with_data => {
                    let (info, data) = self.table_rows(index, table, &mut warnings).await?;
                    entries.insert(data.entry.clone(), info);
                    Some(data)
                }
                _ => None,
            };
            manifest_objects.push(ManifestObject {
                index,
                kind: def.kind(),
                object,
                definition: definition_entry(index),
                ddl: ddl.map(|_| ddl_entry(index)),
                data,
            });
            self.progress.objects_done += 1;
            self.report(true);
        }
        for kind in without_ddl {
            warnings.push(format!(
                "Oxbase has no CREATE text for {kind} objects on {}; the backup holds \
                 their definitions",
                engine.display_name()
            ));
        }
        let order = manifest_objects.iter().map(|o| o.index).collect();
        Ok(Manifest {
            format: FORMAT_NAME.to_owned(),
            format_version: FORMAT_VERSION,
            oxbase_version: env!("CARGO_PKG_VERSION").to_owned(),
            created_at: Utc::now(),
            engine,
            server_version: version.raw().to_owned(),
            source: BackupSource {
                container: names_only(&spec.container),
            },
            content: spec.content,
            compression: spec.compression,
            objects: manifest_objects,
            order,
            entries,
            warnings,
            unknown: serde_json::Map::new(),
        })
    }

    /// Streams `table`'s stored columns into its data entry.
    async fn table_rows(
        &mut self,
        index: u32,
        table: &TableDef,
        warnings: &mut Vec<String>,
    ) -> Result<(EntryInfo, TableData)> {
        let job = self.job;
        let engine = job.rows.engine();
        let version = job.rows.server_version().clone();
        let writer = job.dialects.dml_writer(engine).ok();
        let (mut columns, left_out) = load_columns(table, writer.as_deref(), &version);
        for (column, why) in left_out {
            warnings.push(format!(
                "{}: the values of column {column} were not backed up (the server maintains \
                 them and generates new ones on restore: {why})",
                table.obj.display_name()
            ));
        }
        let quoting = job.dialects.quoting(engine)?;
        let mut w = SqlWriter::new(quoting.as_ref());
        w.push("SELECT ");
        for (i, c) in columns.iter().enumerate() {
            if i > 0 {
                w.push(", ");
            }
            w.ident(&c.name);
        }
        w.push(" FROM ").qualified(&table.obj);
        if let Some(pk) = table
            .primary_key
            .as_ref()
            .filter(|pk| !pk.columns.is_empty())
        {
            w.push(" ORDER BY ");
            for (i, c) in pk.columns.iter().enumerate() {
                if i > 0 {
                    w.push(", ");
                }
                w.ident(c);
            }
        }
        let statement = w.finish();
        if columns.is_empty() {
            return Err(Error::unsupported(format!(
                "{} has no stored columns to back up",
                table.obj.display_name()
            )));
        }

        // The entry starts with the first page: its Arrow types decide which columns keep the
        // server's text form (`keep_text_forms`).
        let mut schema: Option<SchemaRef> = None;
        let tx = until_cancelled(self.cancel, job.rows.begin()).await?;
        let mut stream = until_cancelled(
            self.cancel,
            tx.execute(&statement.sql, &statement.params, self.cancel.clone()),
        )
        .await?;
        let mut rows = 0_u64;
        loop {
            let item = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(Error::Cancelled),
                item = stream.next() => item,
            };
            match item {
                None => break,
                Some(Err(e)) => return Err(e),
                Some(Ok(ResultItem::Batch(batch))) => {
                    if batch.num_rows() == 0 {
                        continue;
                    }
                    let load = if let Some(load) = schema.take() {
                        load
                    } else {
                        keep_text_forms(&mut columns, &batch);
                        let begun = load_schema(&table.obj, &columns, &version);
                        self.archive
                            .begin(index, Arc::clone(&begun), self.cancel)
                            .await?;
                        begun
                    };
                    let converted = to_load_batch(&table.obj, &batch, &load, &columns, rows)?;
                    schema = Some(load);
                    rows += u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
                    self.archive.rows(converted, self.cancel).await?;
                    self.progress.rows += u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
                    self.report(false);
                }
                Some(Ok(_)) => {}
            }
        }
        drop(stream);
        if schema.is_none() {
            self.archive
                .begin(
                    index,
                    load_schema(&table.obj, &columns, &version),
                    self.cancel,
                )
                .await?;
        }
        let (info, written) = self.archive.end(self.cancel).await?;
        // A read-only transaction: ending it either way is fine; a failure here does not touch
        // the rows already read.
        if let Err(e) = tx.commit().await {
            tracing::debug!(error = %e, "ending a backup's read transaction failed");
        }
        debug_assert_eq!(written, rows);
        Ok((
            info,
            TableData {
                entry: data_entry(index),
                rows: written,
                columns: columns.into_iter().map(|c| c.name).collect(),
            },
        ))
    }
}

/// The load schema of `columns` (`LoadTarget::schema`).
fn load_schema(table: &ObjectRef, columns: &[LoadColumn], version: &ServerVersion) -> SchemaRef {
    LoadTarget {
        table,
        columns,
        version,
    }
    .schema()
}

/// A time-zone-aware column the source driver delivers as text (`batch`, the first page) keeps
/// that text in the backup: the engine stores more than the instant the load type holds (SQL
/// Server `datetimeoffset` keeps its offset and 100 ns digits). The restore loads the text and
/// the server parses it back (`restore`, `stored_text_forms`).
fn keep_text_forms(columns: &mut [LoadColumn], batch: &RecordBatch) {
    for (column, field) in columns.iter_mut().zip(batch.schema_ref().fields()) {
        if matches!(column.data_type, DataType::TimestampTz { .. })
            && matches!(field.data_type(), ArrowType::Utf8 | ArrowType::LargeUtf8)
        {
            column.data_type = DataType::Text;
        }
    }
}

/// What a restore of `objects` cannot bring back exactly (module docs, step 5).
fn fidelity_warnings(objects: &[ObjectDef], spec: &BackupSpec) -> Vec<String> {
    let mut out = Vec::new();
    let tables: Vec<&TableDef> = objects
        .iter()
        .filter_map(|d| match d {
            ObjectDef::Table(t) => Some(t),
            _ => None,
        })
        .collect();
    if spec.content.has_rows() {
        for t in &tables {
            for c in &t.columns {
                let precision = match &c.data_type {
                    DataType::Time { precision }
                    | DataType::Timestamp { precision }
                    | DataType::TimestampTz { precision } => *precision,
                    _ => None,
                };
                if precision.is_some_and(|p| p > 6) && c.generated.is_none() {
                    out.push(format!(
                        "{}: column {} holds time values finer than microseconds; the backup \
                         keeps microseconds",
                        t.obj.display_name(),
                        c.name
                    ));
                }
            }
        }
    }
    if objects.iter().any(|d| matches!(d, ObjectDef::Sequence(_))) {
        out.push(
            "the backup does not hold the sequences' current positions; a restore restarts each \
             sequence after the highest restored value of the columns whose default uses it"
                .to_owned(),
        );
    }
    if !spec.objects.is_empty() {
        let here = Location::of_container(&spec.container);
        for t in &tables {
            for fk in &t.foreign_keys {
                let missing = here.contains(&fk.ref_table)
                    && !tables.iter().any(|o| same_table(&fk.ref_table, &o.obj));
                if missing {
                    out.push(format!(
                        "{} has a foreign key to {}, which is not in the backup; restoring it \
                         needs that table in the target",
                        t.obj.display_name(),
                        fk.ref_table.display_name()
                    ));
                }
            }
        }
    }
    out
}

/// The container ref as the manifest keeps it: kind, catalog, schema and name only.
fn names_only(container: &ObjectRef) -> ObjectRef {
    let mut r = ObjectRef::new(container.kind, container.name.clone());
    r.catalog.clone_from(&container.catalog);
    r.schema.clone_from(&container.schema);
    r
}

/// Counts the bytes written through it (progress).
struct Counting<W> {
    inner: W,
    count: Arc<AtomicU64>,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count
            .fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Seek> Seek for Counting<W> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

type Sink = Counting<BufWriter<SinkFile>>;

enum Command {
    Object {
        index: u32,
        definition: Box<ObjectDef>,
        ddl: Option<String>,
        reply: oneshot::Sender<Result<(EntryInfo, Option<EntryInfo>)>>,
    },
    Begin {
        index: u32,
        schema: SchemaRef,
        reply: oneshot::Sender<Result<()>>,
    },
    Rows(RecordBatch),
    End {
        reply: oneshot::Sender<Result<(EntryInfo, u64)>>,
    },
    Finish {
        manifest: Box<Manifest>,
        reply: oneshot::Sender<Result<u64>>,
    },
}

/// The archive writer on a blocking thread, fed through a bounded channel (one batch in
/// flight).
struct ArchiveTask {
    commands: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}

fn stopped() -> Error {
    Error::internal("the backup archive writer stopped unexpectedly")
}

impl ArchiveTask {
    fn spawn(sink: SinkFile, compression: Compression, bytes: Arc<AtomicU64>) -> Self {
        let (commands, receiver) = mpsc::channel(2);
        let task = tokio::task::spawn_blocking(move || {
            let sink = Counting {
                inner: BufWriter::with_capacity(256 * 1024, sink),
                count: bytes,
            };
            write_archive(ArchiveWriter::new(sink, compression), receiver);
        });
        Self { commands, task }
    }

    async fn call<T>(
        &self,
        cancel: &CancellationToken,
        command: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
    ) -> Result<T> {
        let (reply, answer) = oneshot::channel();
        until_cancelled(cancel, async {
            self.commands
                .send(command(reply))
                .await
                .map_err(|_| stopped())
        })
        .await?;
        until_cancelled(cancel, async { answer.await.map_err(|_| stopped())? }).await
    }

    async fn object(
        &self,
        index: u32,
        definition: ObjectDef,
        ddl: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<(EntryInfo, Option<EntryInfo>)> {
        self.call(cancel, |reply| Command::Object {
            index,
            definition: Box::new(definition),
            ddl,
            reply,
        })
        .await
    }

    async fn begin(&self, index: u32, schema: SchemaRef, cancel: &CancellationToken) -> Result<()> {
        self.call(cancel, |reply| Command::Begin {
            index,
            schema,
            reply,
        })
        .await
    }

    async fn rows(&self, batch: RecordBatch, cancel: &CancellationToken) -> Result<()> {
        until_cancelled(cancel, async {
            self.commands
                .send(Command::Rows(batch))
                .await
                .map_err(|_| stopped())
        })
        .await
    }

    async fn end(&self, cancel: &CancellationToken) -> Result<(EntryInfo, u64)> {
        self.call(cancel, |reply| Command::End { reply }).await
    }

    /// Writes the manifest, closes the ZIP and renames the file; returns its size. Not
    /// cancellable: once the manifest is written the commit is a rename.
    async fn finish(&self, manifest: Manifest) -> Result<u64> {
        self.call(&CancellationToken::new(), |reply| Command::Finish {
            manifest: Box::new(manifest),
            reply,
        })
        .await
    }

    /// Stops the writer thread and waits for it (an uncommitted file is deleted on the way).
    async fn close(self) {
        // Closing the channel ends the thread's loop.
        drop(self.commands);
        let _ = self.task.await;
    }
}

/// The writer thread: one command at a time until the channel closes or the archive is done.
/// The first failure is kept and answered to every later command that expects an answer.
fn write_archive(mut writer: ArchiveWriter<Sink>, mut commands: mpsc::Receiver<Command>) {
    let mut failed: Option<Error> = None;
    while let Some(command) = commands.blocking_recv() {
        match command {
            Command::Object {
                index,
                definition,
                ddl,
                reply,
            } => {
                let result = match &failed {
                    Some(e) => Err(e.clone()),
                    None => writer.add_object(index, &definition, ddl.as_deref()),
                };
                let _ = reply.send(result);
            }
            Command::Begin {
                index,
                schema,
                reply,
            } => {
                let result = match &failed {
                    Some(e) => Err(e.clone()),
                    None => writer.begin_table(index, &schema),
                };
                if let Err(e) = &result {
                    failed.get_or_insert_with(|| e.clone());
                }
                let _ = reply.send(result);
            }
            Command::Rows(batch) => {
                if failed.is_none()
                    && let Err(e) = writer.write_rows(&batch)
                {
                    failed = Some(e);
                }
            }
            Command::End { reply } => {
                let result = match &failed {
                    Some(e) => Err(e.clone()),
                    None => writer.end_table(),
                };
                let _ = reply.send(result);
            }
            Command::Finish { manifest, reply } => {
                let result = match failed {
                    Some(e) => Err(e),
                    None => commit(writer, &manifest),
                };
                let _ = reply.send(result);
                return;
            }
        }
    }
    // The channel closed before `Finish`: dropping the writer deletes the temporary file.
}

fn commit(writer: ArchiveWriter<Sink>, manifest: &Manifest) -> Result<u64> {
    let counting = writer.finish(manifest)?;
    let sink = counting
        .inner
        .into_inner()
        .map_err(|e| Error::from(e.into_error()))?;
    sink.commit()
}
