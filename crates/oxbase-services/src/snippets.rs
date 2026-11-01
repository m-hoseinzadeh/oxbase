//! M6: snippets for the editor (`docs/m6-contracts.md` §6): the built-in set of
//! `oxbase_insight::snippets` plus the user's own in a
//! [`SnippetRepository`](oxbase_store::SnippetRepository). Complete (Design step).

use std::sync::Arc;

use oxbase_store::SnippetRepository;
use oxbase_types::{Engine, Error, Result, Snippet, SnippetInput};
use uuid::Uuid;

/// Lists, saves and deletes snippets.
#[derive(Debug, Clone)]
pub struct SnippetService {
    repo: Arc<dyn SnippetRepository>,
}

impl SnippetService {
    pub fn new(repo: Arc<dyn SnippetRepository>) -> Self {
        Self { repo }
    }

    pub fn repository(&self) -> &Arc<dyn SnippetRepository> {
        &self.repo
    }

    /// The user's snippets (by prefix) followed by the built-in ones, keeping only those that
    /// apply to `engine` when it is given. MongoDB and Redis have none.
    pub async fn list(&self, engine: Option<Engine>) -> Result<Vec<Snippet>> {
        if matches!(engine, Some(Engine::MongoDb | Engine::Redis)) {
            return Ok(Vec::new());
        }
        let mut out = self.repo.list().await?;
        out.extend(oxbase_insight::snippets::builtin_snippets());
        if let Some(engine) = engine {
            out.retain(|s| s.applies_to(engine));
        }
        Ok(out)
    }

    /// Creates or replaces a user snippet (validated by the repository).
    pub async fn save(&self, input: &SnippetInput) -> Result<Snippet> {
        self.repo.save(input).await
    }

    /// Deletes a user snippet by id. Built-in snippets (`builtin:...`) cannot be deleted
    /// (`InvalidInput`); an unknown id gives `false`.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        if id.starts_with("builtin:") {
            return Err(Error::invalid_input("built-in snippets cannot be deleted"));
        }
        let id = Uuid::parse_str(id).map_err(|_| Error::invalid_input("not a snippet id"))?;
        self.repo.delete(id).await
    }
}

#[cfg(test)]
mod tests {
    use oxbase_store::MemorySnippetRepository;
    use oxbase_types::ErrorCategory;

    use super::*;

    #[tokio::test]
    async fn lists_user_snippets_first_and_filters_by_engine() {
        let service = SnippetService::new(Arc::new(MemorySnippetRepository::new()));
        let builtins = oxbase_insight::snippets::builtin_snippets();
        assert_eq!(service.list(None).await.unwrap(), builtins);
        let mine = service
            .save(&SnippetInput {
                id: None,
                name: "Mine".into(),
                prefix: "zz".into(),
                body: "SELECT 42".into(),
                description: String::new(),
                engines: vec![Engine::Sqlite],
            })
            .await
            .unwrap();
        let sqlite = service.list(Some(Engine::Sqlite)).await.unwrap();
        assert_eq!(sqlite[0], mine);
        assert!(sqlite.iter().all(|s| s.applies_to(Engine::Sqlite)));
        let pg = service.list(Some(Engine::Postgres)).await.unwrap();
        assert!(pg.iter().all(|s| s.builtin));
        assert_eq!(service.list(Some(Engine::Redis)).await.unwrap(), Vec::new());

        let err = service.delete("builtin:select").await.unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput);
        let err = service.delete("nope").await.unwrap_err();
        assert_eq!(err.category(), ErrorCategory::InvalidInput);
        assert!(service.delete(&mine.id).await.unwrap());
        assert!(!service.delete(&mine.id).await.unwrap());
    }
}
