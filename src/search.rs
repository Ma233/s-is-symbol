use std::env;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use onoma::indexer::DatabaseBackedIndexer;
use onoma::indexer::Indexer;
use onoma::models::resolved::ResolvedSymbol;
use onoma::resolver::Context as QueryContext;
use onoma::resolver::DatabaseBackedResolver;
use onoma::resolver::Resolver;
use tokio_stream::StreamExt;

pub(crate) struct OnomaSearch {
    storage: PathBuf,
    workspace: PathBuf,
}

impl OnomaSearch {
    pub(crate) fn new(workspace: PathBuf) -> Result<Self> {
        let storage = cache_dir().context("cannot determine a cache directory")?;
        Ok(Self { storage, workspace })
    }

    pub(crate) async fn find(&self, query: &str) -> Result<Vec<ResolvedSymbol>> {
        let workspaces = [self.workspace.as_path()];
        let indexer = DatabaseBackedIndexer::new(&self.storage, workspaces)
            .await
            .context("cannot initialize the Onoma index")?;

        indexer
            .deindex(&self.workspace)
            .await
            .context("cannot clear the existing Onoma index")?;

        if let Err(errors) = indexer.index_workspaces().await {
            let details = errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            bail!("Onoma could not index the workspace: {details}");
        }

        let resolver = DatabaseBackedResolver::new(&self.storage, workspaces);
        let mut stream = resolver.query(query.to_owned(), QueryContext::default());
        let mut results = Vec::new();
        while let Some(symbol) = stream.next().await {
            results.push(symbol);
        }

        retain_best_name_matches(query, &mut results);
        results.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.start_line.cmp(&right.start_line))
        });
        Ok(results)
    }
}

fn retain_best_name_matches(query: &str, results: &mut Vec<ResolvedSymbol>) {
    if results.iter().any(|symbol| symbol.name == query) {
        results.retain(|symbol| symbol.name == query);
        return;
    }

    if results
        .iter()
        .any(|symbol| symbol.name.eq_ignore_ascii_case(query))
    {
        results.retain(|symbol| symbol.name.eq_ignore_ascii_case(query));
    }
}

fn cache_dir() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|path| path.join("symbol").join("onoma"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_a_real_rust_function_declaration() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        std::fs::write(
            workspace.path().join("sample.rs"),
            "pub fn target_function() {}\n",
        )?;
        let storage = workspace.path().join("cache");
        let search = OnomaSearch {
            storage,
            workspace: workspace.path().to_path_buf(),
        };

        let results = search.find("target_function").await?;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "target_function");
        assert_eq!(results[0].start_line, 1);
        Ok(())
    }

    #[tokio::test]
    async fn prefers_one_exact_declaration_over_fuzzy_matches() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        std::fs::write(
            workspace.path().join("sample.rs"),
            "pub struct Cli;\npub struct Client;\npub struct Clippy;\n",
        )?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        let results = search.find("Cli").await?;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Cli");
        Ok(())
    }

    #[tokio::test]
    async fn keeps_duplicate_exact_declarations_for_the_picker() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        std::fs::write(workspace.path().join("first.rs"), "pub struct Cli;\n")?;
        std::fs::write(
            workspace.path().join("second.rs"),
            "pub struct Cli;\npub struct Client;\n",
        )?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        let results = search.find("Cli").await?;

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|symbol| symbol.name == "Cli"));
        Ok(())
    }

    #[tokio::test]
    async fn ignores_stale_symbols_for_deleted_files() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let first = workspace.path().join("first.rs");
        let second = workspace.path().join("second.rs");
        std::fs::write(&first, "pub struct Cli;\n")?;
        std::fs::write(&second, "pub struct Cli;\n")?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        assert_eq!(search.find("Cli").await?.len(), 2);
        std::fs::remove_file(second)?;

        let results = search.find("Cli").await?;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, first);
        Ok(())
    }

    #[tokio::test]
    async fn removes_symbols_that_become_gitignored() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let target = workspace.path().join("target");
        std::fs::create_dir(&target)?;
        std::fs::write(target.join("generated.rs"), "pub struct Generated;\n")?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        assert_eq!(search.find("Generated").await?.len(), 1);
        std::fs::write(workspace.path().join(".gitignore"), "/target/\n")?;

        assert!(search.find("Generated").await?.is_empty());
        Ok(())
    }
}
