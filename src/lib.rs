//! Command-line orchestration for finding declarations and opening Neovim.

mod neovim;
mod search;

use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;

use crate::neovim::Neovim;
use crate::search::OnomaSearch;

/// Find a declaration and jump to it in Neovim.
#[derive(Debug, Parser)]
#[command(name = "s", version, about)]
pub struct Cli {
    /// Function, class, struct, or other declaration name to find.
    pub query: String,

    /// Workspace to search.
    #[arg(default_value = ".", value_name = "PATH")]
    pub path: PathBuf,

    /// Neovim executable to launch.
    #[arg(
        long,
        default_value = "nvim",
        env = "SYMBOL_NVIM",
        value_name = "COMMAND"
    )]
    pub nvim: PathBuf,
}

/// Executes an `s` invocation.
///
/// # Errors
///
/// Returns an error when the workspace cannot be resolved or indexed, or when
/// Neovim cannot be started successfully.
pub async fn run(cli: Cli) -> Result<()> {
    let query = cli.query.trim();
    if query.is_empty() {
        bail!("query must not be empty");
    }

    let workspace = cli
        .path
        .canonicalize()
        .with_context(|| format!("cannot resolve workspace {}", cli.path.display()))?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }

    let search = OnomaSearch::new(workspace.clone())?;
    let results = search.find(query).await?;
    let editor = Neovim::new(cli.nvim, workspace);

    match results.as_slice() {
        [result] if result.name.eq_ignore_ascii_case(query) => editor.open_location(result),
        _ => editor.open_search(query),
    }
}
