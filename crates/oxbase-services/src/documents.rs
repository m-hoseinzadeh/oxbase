//! Documents (M7): the find / aggregate builder's runs, single documents for the tree and JSON
//! views, and optimistic document edits, through the engine's
//! [`DocumentStore`](oxbase_dialect::DocumentStore). No engine special case: everything
//! MongoDB-specific is in the registered cell. Contract: `docs/m7-contracts.md` §3.1, §5.1, §8.
//!
//! - [`DocumentService::open`] validates a [`DocumentQuery`] and starts it as a query run
//!   ([`QueryService::run_source`]) in the caller's editor: events, `query_page`, cancel and
//!   close as for `query_run`. The run's statement text is the request as shell code
//!   (`DocumentStore::request_text`), recorded in the history unless the options turn it off.
//! - [`DocumentService::document_at`] reads the whole document of one row of such a run from
//!   its document column (`oxbase_dialect::document::DOCUMENT_COLUMN`), as stored when the
//!   run read it; the UI overlays its own applied edits (from the apply reports).
//! - [`DocumentService::get`] reads one document by `_id` on the session the tree uses for its
//!   database, [`DocumentService::apply`] applies edits there (see
//!   `oxbase_types::document::DocumentEdit` for the optimistic checks). Both hold
//!   `SessionHandle::exclusive` while they talk to the server.

use std::sync::Arc;

use arrow_array::Array;
use arrow_array::cast::AsArray;

use oxbase_dialect::document::{META_ROLE, ROLE_DOCUMENT};
use oxbase_types::document::validate_document_edits;
use oxbase_types::{DocumentApplyReport, DocumentEdit, DocumentQuery, Error, ObjectRef, Result};

use crate::query::{QueryService, RunHandle, RunId, RunOptions, RunTarget};
use crate::tree::TreeService;
use crate::{CancellationToken, SessionId, check_cancelled};

/// See the module docs.
pub struct DocumentService {
    query: Arc<QueryService>,
    tree: Arc<TreeService>,
}

impl DocumentService {
    pub fn new(query: Arc<QueryService>, tree: Arc<TreeService>) -> Self {
        Self { query, tree }
    }

    /// Starts `request` in editor `target.editor` of base session `target.session` (see the
    /// module docs). `InvalidInput` for a malformed request, `Unsupported` for an engine
    /// without documents.
    pub async fn open(
        &self,
        target: RunTarget,
        request: &DocumentQuery,
        options: RunOptions,
        cancel: CancellationToken,
    ) -> Result<RunHandle> {
        check_cancelled(&cancel)?;
        request.validate()?;
        let base = self
            .query
            .manager()
            .get(target.session)
            .ok_or_else(|| Error::not_found(format!("no open session {}", target.session)))?;
        let store = self
            .query
            .dialects()
            .document_store(base.summary().engine)?;
        let source = store.query(request)?;
        let text = store.request_text(request);
        check_cancelled(&cancel)?;
        self.query.run_source(target, text, source, options).await
    }

    /// The whole document of row `row` of result `result_index` of run `run`, as canonical
    /// Extended JSON text. `NotFound` past the stored rows, `InvalidInput` when the result has
    /// no document column (not a document run).
    pub async fn document_at(&self, run: RunId, result_index: usize, row: u64) -> Result<String> {
        let batch = self.query.page(run, result_index, row, 1).await?;
        let schema = batch.schema();
        let column = schema
            .fields()
            .iter()
            .position(|f| {
                f.metadata()
                    .get(META_ROLE)
                    .is_some_and(|role| role == ROLE_DOCUMENT)
            })
            .ok_or_else(|| Error::invalid_input("this result holds no documents"))?;
        if batch.num_rows() == 0 {
            return Err(Error::not_found(format!(
                "query run {run} has no row {row} in result {result_index}"
            )));
        }
        batch
            .column(column)
            .as_string_opt::<i32>()
            .filter(|texts| !texts.is_null(0))
            .map(|texts| texts.value(0).to_owned())
            .ok_or_else(|| Error::internal("the document column holds no text"))
    }

    /// The document of `collection` whose `_id` is `id`; `None` when there is none.
    pub async fn get(
        &self,
        session: SessionId,
        collection: &ObjectRef,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<Option<String>> {
        check_cancelled(&cancel)?;
        let handle = self
            .tree
            .session_for(session, collection.database_name(), cancel.clone())
            .await?;
        let store = self
            .query
            .dialects()
            .document_store(handle.summary().engine)?;
        let _guard = handle.exclusive().await;
        store
            .get(handle.connection().as_ref(), collection, id, cancel)
            .await
    }

    /// Applies `edits` to `collection` in order (see the module docs); per-edit outcomes in the
    /// report. `InvalidInput` (nothing written) when an edit is malformed.
    pub async fn apply(
        &self,
        session: SessionId,
        collection: &ObjectRef,
        edits: &[DocumentEdit],
        cancel: CancellationToken,
    ) -> Result<DocumentApplyReport> {
        check_cancelled(&cancel)?;
        validate_document_edits(edits)?;
        let handle = self
            .tree
            .session_for(session, collection.database_name(), cancel.clone())
            .await?;
        let store = self
            .query
            .dialects()
            .document_store(handle.summary().engine)?;
        let _guard = handle.exclusive().await;
        store
            .apply(handle.connection().as_ref(), collection, edits, cancel)
            .await
    }
}
