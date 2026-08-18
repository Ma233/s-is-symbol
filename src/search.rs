use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Read as _;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
#[cfg(test)]
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use fs2::FileExt as _;
use onoma::indexer::DatabaseBackedIndexer;
use onoma::indexer::Indexer;
use onoma::models::parsed::Language;
use onoma::models::resolved::ResolvedSymbol;
use onoma::resolver::Context as QueryContext;
use onoma::resolver::DatabaseBackedResolver;
use onoma::resolver::Resolver;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqlitePoolOptions;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;

const INDEX_STATE_VERSION: u8 = 3;
const FILES_PER_UPDATE_TASK: usize = 64;
const AUTOMATIC_GC_INTERVAL_SECS: u64 = 24 * 60 * 60;
const GC_GRACE_PERIOD_SECS: u64 = 7 * 24 * 60 * 60;

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
        let paths = IndexStatePaths::new(&self.storage, &self.workspace);
        let _lock = acquire_index_lock(paths.lock.clone()).await?;
        let database_existed = paths.database.exists();
        let previous = if database_existed && !paths.dirty.exists() {
            load_index_state(&paths.manifest)?
        } else {
            None
        };
        let fingerprint = git_workspace_fingerprint(&self.workspace)?;
        if previous
            .as_ref()
            .is_none_or(|state| state.fingerprint != fingerprint || fingerprint.is_none())
        {
            let indexer = DatabaseBackedIndexer::new(&self.storage, workspaces)
                .await
                .context("cannot initialize the Onoma index")?;
            self.sync_index(&indexer, &paths, previous, fingerprint)
                .await?;
        }
        ensure_workspace_record(&paths.workspace, &self.workspace)?;
        let _cleanup_result = maybe_gc_cache(&self.storage, false);

        let mut exact = query_exact_name(&paths.database, query).await?;
        if !exact.is_empty() {
            retain_best_name_matches(query, &mut exact);
            sort_results(&mut exact);
            return Ok(exact);
        }

        let resolver = DatabaseBackedResolver::new(&self.storage, workspaces);
        let mut stream = resolver.query(query.to_owned(), QueryContext::default());
        let mut results = Vec::new();
        while let Some(symbol) = stream.next().await {
            results.push(symbol);
        }

        retain_best_name_matches(query, &mut results);
        sort_results(&mut results);
        Ok(results)
    }

    async fn sync_index(
        &self,
        indexer: &DatabaseBackedIndexer,
        paths: &IndexStatePaths,
        previous: Option<IndexState>,
        fingerprint: Option<String>,
    ) -> Result<()> {
        let current = build_git_index_state(&self.workspace, previous.as_ref(), fingerprint)?;

        let Some(previous) = previous else {
            mark_dirty(&paths.dirty)?;
            indexer
                .deindex(&self.workspace)
                .await
                .context("cannot clear the existing Onoma index")?;
            let files = current.files.keys().cloned().collect::<Vec<_>>();
            index_changed_files(indexer, &self.workspace, files).await?;
            save_index_state(paths, &current)?;
            return Ok(());
        };

        let removed = previous
            .files
            .keys()
            .filter(|path| !current.files.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        let changed = current
            .files
            .iter()
            .filter(|(path, state)| previous.files.get(*path) != Some(*state))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        if removed.is_empty() && changed.is_empty() {
            mark_dirty(&paths.dirty)?;
            return save_index_state(paths, &current);
        }

        mark_dirty(&paths.dirty)?;
        deindex_exact_files(&paths.database, &self.workspace, &removed).await?;
        index_changed_files(indexer, &self.workspace, changed).await?;
        save_index_state(paths, &current)
    }
}

async fn deindex_exact_files(database: &Path, workspace: &Path, files: &[PathBuf]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let options = SqliteConnectOptions::new().filename(database);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .context("cannot open the Onoma index for removing stale files")?;
    let mut transaction = pool
        .begin()
        .await
        .context("cannot begin an Onoma index cleanup transaction")?;
    for file in files {
        let path = workspace.join(file);
        let path = path.to_string_lossy();
        sqlx::query("DELETE FROM symbol WHERE file_id IN (SELECT id FROM file WHERE path = ?)")
            .bind(path.as_ref())
            .execute(&mut *transaction)
            .await
            .with_context(|| format!("cannot remove stale symbols for {path}"))?;
        sqlx::query("DELETE FROM file WHERE path = ?")
            .bind(path.as_ref())
            .execute(&mut *transaction)
            .await
            .with_context(|| format!("cannot remove stale file {path} from the index"))?;
    }
    transaction
        .commit()
        .await
        .context("cannot commit the Onoma index cleanup")
}

async fn query_exact_name(database: &Path, query: &str) -> Result<Vec<ResolvedSymbol>> {
    let options = SqliteConnectOptions::new()
        .filename(database)
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .context("cannot open the Onoma index for an exact query")?;
    sqlx::query_as::<_, ResolvedSymbol>(
        "SELECT symbol.id, symbol.kind, symbol.language, file.path, symbol.name, \
         symbol.start_line, symbol.end_line, symbol.start_column, symbol.end_column \
         FROM symbol INNER JOIN file ON symbol.file_id = file.id \
         WHERE symbol.name = ? COLLATE NOCASE",
    )
    .bind(query)
    .fetch_all(&pool)
    .await
    .context("cannot query the Onoma index by exact name")
}

fn sort_results(results: &mut [ResolvedSymbol]) {
    results.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.start_line.cmp(&right.start_line))
    });
}

#[derive(Debug, Deserialize, Serialize)]
struct IndexState {
    version: u8,
    files: BTreeMap<PathBuf, FileState>,
    head: Option<String>,
    dirty_files: BTreeSet<PathBuf>,
    fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileState {
    digest: String,
}

struct IndexStatePaths {
    database: PathBuf,
    dirty: PathBuf,
    manifest: PathBuf,
    lock: PathBuf,
    workspace: PathBuf,
}

impl IndexStatePaths {
    fn new(storage: &Path, workspace: &Path) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(workspace.to_str().unwrap_or_default().as_bytes());
        let key = hex::encode(hasher.finalize());
        Self {
            database: storage.join(format!("{key}.db")),
            dirty: storage.join(format!("{key}.dirty")),
            manifest: storage.join(format!("{key}.json")),
            lock: storage.join(format!("{key}.lock")),
            workspace: storage.join(format!("{key}.workspace")),
        }
    }
}

async fn acquire_index_lock(path: PathBuf) -> Result<std::fs::File> {
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("cannot create the Onoma cache directory")?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(path)
            .context("cannot open the workspace index lock")?;
        file.lock_exclusive()
            .context("cannot lock the workspace index")?;
        Result::<std::fs::File>::Ok(file)
    })
    .await
    .context("workspace index lock task failed")?
}

fn build_git_index_state(
    workspace: &Path,
    previous: Option<&IndexState>,
    fingerprint: Option<String>,
) -> Result<IndexState> {
    let root = PathBuf::from(git_text(workspace, &["rev-parse", "--show-toplevel"])?);
    let head = git_optional_text(workspace, &["rev-parse", "--verify", "HEAD"])?;
    let listed = git_output(workspace, &[
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "--full-name",
        "-z",
    ])?;
    let all_files = git_paths(&root, workspace, &listed)?;

    let mut dirty_files = if head.is_some() {
        let changed = git_output(workspace, &["diff", "--name-only", "-z", "HEAD", "--"])?;
        git_paths(&root, workspace, &changed)?
    } else {
        all_files.clone()
    };
    let untracked = git_output(workspace, &[
        "ls-files",
        "--others",
        "--exclude-standard",
        "--full-name",
        "-z",
    ])?;
    dirty_files.extend(git_paths(&root, workspace, &untracked)?);
    dirty_files.retain(|path| all_files.contains(path));

    let head_changed = previous.is_none_or(|state| state.head != head);
    let mut files = BTreeMap::new();
    for path in &all_files {
        let must_hash = head_changed
            || previous.is_none_or(|state| {
                state.dirty_files.contains(path)
                    || dirty_files.contains(path)
                    || !state.files.contains_key(path)
            });
        let state = if must_hash {
            FileState::from_file(&workspace.join(path))?
        } else {
            previous
                .and_then(|state| state.files.get(path))
                .context("missing cached source file digest")?
                .clone()
        };
        files.insert(path.clone(), state);
    }
    Ok(IndexState {
        version: INDEX_STATE_VERSION,
        files,
        head,
        dirty_files,
        fingerprint,
    })
}

fn git_paths(root: &Path, workspace: &Path, output: &[u8]) -> Result<BTreeSet<PathBuf>> {
    let workspace = workspace
        .canonicalize()
        .context("cannot resolve the Git workspace path")?;
    let mut paths = BTreeSet::new();
    for bytes in output
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
    {
        #[cfg(unix)]
        let relative = git_path_from_bytes(bytes);
        #[cfg(not(unix))]
        let relative = git_path_from_bytes(bytes)?;
        let absolute = root.join(relative);
        if absolute.to_str().is_none()
            || !absolute.is_file()
            || Language::try_from(absolute.as_path()).is_err()
        {
            continue;
        }
        if let Ok(relative) = absolute.strip_prefix(&workspace) {
            paths.insert(relative.to_path_buf());
        }
    }
    Ok(paths)
}

#[cfg(unix)]
fn git_path_from_bytes(bytes: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn git_path_from_bytes(bytes: &[u8]) -> Result<&Path> {
    Ok(Path::new(
        std::str::from_utf8(bytes).context("Git returned a non-UTF-8 path")?,
    ))
}

impl FileState {
    fn from_file(path: &Path) -> Result<Self> {
        let mut file = File::open(path)
            .with_context(|| format!("cannot open source file {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("cannot hash source file {}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(Self {
            digest: hex::encode(hasher.finalize()),
        })
    }
}

fn git_workspace_fingerprint(workspace: &Path) -> Result<Option<String>> {
    let status = git_output(workspace, &[
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--untracked-files=all",
    ])?;
    if status
        .split(|byte| *byte == 0)
        .any(|record| !record.is_empty() && !record.starts_with(b"# "))
    {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    hash_bytes(&mut hasher, &status);
    hash_optional_git_output(
        &mut hasher,
        git_optional_output(workspace, &["config", "--get", "core.sparseCheckout"])?,
    );
    hash_optional_git_output(
        &mut hasher,
        git_optional_output(workspace, &["config", "--get", "core.sparseCheckoutCone"])?,
    );
    let sparse_path = git_output(workspace, &[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "info/sparse-checkout",
    ])?;
    let sparse_path = sparse_path
        .strip_suffix(b"\n")
        .unwrap_or(sparse_path.as_slice());
    #[cfg(unix)]
    let sparse_path = git_path_from_bytes(sparse_path);
    #[cfg(not(unix))]
    let sparse_path = git_path_from_bytes(sparse_path)?;
    let sparse_rules = match std::fs::read(sparse_path) {
        Ok(rules) => Some(rules),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("cannot read the Git sparse-checkout rules"),
    };
    hash_optional_git_output(&mut hasher, sparse_rules);
    Ok(Some(hex::encode(hasher.finalize())))
}

fn hash_optional_git_output(hasher: &mut Sha256, output: Option<Vec<u8>>) {
    match output {
        Some(output) => {
            hasher.update([1]);
            hash_bytes(hasher, &output);
        }
        None => hasher.update([0]),
    }
}

fn git_output(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stderr(Stdio::null())
        .output()
        .context("cannot run Git")?;
    anyhow::ensure!(
        output.status.success(),
        "workspace is not inside a Git working tree"
    );
    Ok(output.stdout)
}

fn git_text(workspace: &Path, args: &[&str]) -> Result<String> {
    let output = git_output(workspace, args)?;
    Ok(std::str::from_utf8(&output)
        .context("Git returned non-UTF-8 output")?
        .trim()
        .to_owned())
}

fn git_optional_text(workspace: &Path, args: &[&str]) -> Result<Option<String>> {
    let Some(output) = git_optional_output(workspace, args)? else {
        return Ok(None);
    };
    Ok(Some(
        std::str::from_utf8(&output)
            .context("Git returned non-UTF-8 output")?
            .trim()
            .to_owned(),
    ))
}

fn git_optional_output(workspace: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stderr(Stdio::null())
        .output()
        .context("cannot run Git")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

fn hash_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(bytes.len().to_le_bytes());
    hasher.update(bytes);
}

fn load_index_state(path: &Path) -> Result<Option<IndexState>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot read the Onoma index state"),
    };
    let state = match serde_json::from_slice::<IndexState>(&bytes) {
        Ok(state) if state.version == INDEX_STATE_VERSION => state,
        Ok(_) | Err(_) => return Ok(None),
    };
    Ok(Some(state))
}

fn mark_dirty(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("cannot create the Onoma cache directory")?;
    }
    std::fs::write(path, []).context("cannot mark the Onoma index as dirty")
}

fn save_index_state(paths: &IndexStatePaths, state: &IndexState) -> Result<()> {
    let temporary = paths
        .manifest
        .with_extension(format!("json.{}", std::process::id()));
    let bytes = serde_json::to_vec(state).context("cannot encode the Onoma index state")?;
    std::fs::write(&temporary, bytes).context("cannot write the Onoma index state")?;
    std::fs::rename(&temporary, &paths.manifest).context("cannot publish the Onoma index state")?;
    std::fs::remove_file(&paths.dirty).context("cannot mark the Onoma index as clean")
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkspaceRecord {
    path: PathBuf,
    missing_since: Option<u64>,
}

fn ensure_workspace_record(path: &Path, workspace: &Path) -> Result<()> {
    if let Ok(bytes) = std::fs::read(path)
        && let Ok(record) = serde_json::from_slice::<WorkspaceRecord>(&bytes)
        && record.path == workspace
        && record.missing_since.is_none()
    {
        return Ok(());
    }
    write_workspace_record(path, &WorkspaceRecord {
        path: workspace.to_path_buf(),
        missing_since: None,
    })
}

fn write_workspace_record(path: &Path, record: &WorkspaceRecord) -> Result<()> {
    let bytes = serde_json::to_vec(record).context("cannot encode workspace cache metadata")?;
    std::fs::write(path, bytes).context("cannot write workspace cache metadata")
}

pub(crate) fn force_gc() -> Result<usize> {
    let storage = cache_dir().context("cannot determine a cache directory")?;
    gc_cache(&storage, true, SystemTime::now())
}

fn maybe_gc_cache(storage: &Path, force: bool) -> Result<usize> {
    gc_cache(storage, force, SystemTime::now())
}

fn gc_cache(storage: &Path, force: bool, now: SystemTime) -> Result<usize> {
    std::fs::create_dir_all(storage).context("cannot create the Onoma cache directory")?;
    let marker = storage.join(".last_gc");
    if !force
        && marker
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|elapsed| elapsed.as_secs() < AUTOMATIC_GC_INTERVAL_SECS)
    {
        return Ok(0);
    }

    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .context("system time is before the Unix epoch")?
        .as_secs();
    let mut removed = 0;
    for entry in std::fs::read_dir(storage).context("cannot inspect the Onoma cache directory")? {
        let entry = entry.context("cannot inspect a workspace cache entry")?;
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "workspace")
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(mut record) = serde_json::from_slice::<WorkspaceRecord>(&bytes) else {
            continue;
        };
        if record.path.exists() {
            if record.missing_since.take().is_some() {
                write_workspace_record(&path, &record)?;
            }
            continue;
        }
        let missing_since = *record.missing_since.get_or_insert(now_secs);
        if force || now_secs.saturating_sub(missing_since) >= GC_GRACE_PERIOD_SECS {
            remove_workspace_cache(storage, &path)?;
            removed += 1;
        } else {
            write_workspace_record(&path, &record)?;
        }
    }
    std::fs::write(marker, []).context("cannot record the last cache cleanup time")?;
    Ok(removed)
}

fn remove_workspace_cache(storage: &Path, workspace_record: &Path) -> Result<()> {
    let key = workspace_record
        .file_stem()
        .context("workspace cache metadata has no key")?
        .to_string_lossy();
    for suffix in [
        "db",
        "db-wal",
        "db-shm",
        "json",
        "dirty",
        "lock",
        "workspace",
    ] {
        let path = storage.join(format!("{key}.{suffix}"));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot remove stale cache file {}", path.display()));
            }
        }
    }
    Ok(())
}

async fn index_changed_files(
    indexer: &DatabaseBackedIndexer,
    workspace: &Path,
    changed: Vec<PathBuf>,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    for chunk in changed.chunks(FILES_PER_UPDATE_TASK) {
        let indexer = indexer.clone();
        let paths = chunk
            .iter()
            .map(|path| workspace.join(path))
            .collect::<Vec<_>>();
        tasks.spawn(async move {
            for path in paths {
                indexer.index(&path).await.with_context(|| {
                    format!("cannot update {} in the Onoma index", path.display())
                })?;
            }
            Result::<()>::Ok(())
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.context("an Onoma indexing task failed")??;
    }
    Ok(())
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
        init_git_workspace(workspace.path())?;
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
        init_git_workspace(workspace.path())?;
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
    async fn reuses_the_index_when_workspace_files_are_unchanged() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        init_git_workspace(workspace.path())?;
        std::fs::write(workspace.path().join("sample.rs"), "pub struct Target;\n")?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        let first = search.find("Target").await?;
        let second = search.find("Target").await?;

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].id, second[0].id);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serializes_concurrent_index_updates() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        init_git_workspace(workspace.path())?;
        std::fs::write(workspace.path().join("sample.rs"), "pub struct Target;\n")?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        let (left, right) = tokio::join!(search.find("Target"), search.find("Target"));

        assert_eq!(left?.len(), 1);
        assert_eq!(right?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn reindexes_only_a_source_file_that_changed() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        init_git_workspace(workspace.path())?;
        let source = workspace.path().join("sample.rs");
        std::fs::write(&source, "pub struct Old;\n")?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        assert_eq!(search.find("Old").await?.len(), 1);
        std::fs::write(&source, "pub struct Replacement;\n")?;

        assert!(search.find("Old").await?.is_empty());
        assert_eq!(search.find("Replacement").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn reindexes_same_size_content_with_a_preserved_mtime() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        init_git_workspace(workspace.path())?;
        let source = workspace.path().join("sample.rs");
        std::fs::write(&source, "pub struct Old;\n")?;
        run_git(workspace.path(), ["add", "sample.rs"])?;
        run_git(workspace.path(), [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "test fixture",
        ])?;
        let original_mtime =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&source)?);
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        assert_eq!(search.find("Old").await?.len(), 1);
        std::fs::write(&source, "pub struct New;\n")?;
        filetime::set_file_mtime(&source, original_mtime)?;

        assert!(search.find("Old").await?.is_empty());
        assert_eq!(search.find("New").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_a_non_git_workspace() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        std::fs::write(workspace.path().join("sample.rs"), "pub struct Target;\n")?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        let error = search
            .find("Target")
            .await
            .err()
            .context("non-Git workspace was accepted")?;

        assert!(error.to_string().contains("Git working tree"));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn indexes_a_git_workspace_containing_a_non_utf8_path() -> Result<()> {
        use std::os::unix::ffi::OsStringExt as _;

        let workspace = tempfile::tempdir()?;
        init_git_workspace(workspace.path())?;
        let file_name = std::ffi::OsString::from_vec(b"asset\xff.bin".to_vec());
        std::fs::write(workspace.path().join(file_name), [])?;
        std::fs::write(
            workspace.path().join("sample.rs"),
            "pub struct Utf8Target;\n",
        )?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        let results = search.find("Utf8Target").await?;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Utf8Target");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_bytes_from_git_paths() {
        use std::os::unix::ffi::OsStrExt as _;

        let bytes = b"asset\xff.bin";

        assert_eq!(git_path_from_bytes(bytes).as_os_str().as_bytes(), bytes);
    }

    #[test]
    fn scans_source_files_in_a_git_worktree() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let repository = directory.path().join("repository");
        let worktree = directory.path().join("worktree");
        std::fs::create_dir(&repository)?;
        run_git(&repository, ["init"])?;
        std::fs::write(repository.join("sample.rs"), "pub struct Target;\n")?;
        run_git(&repository, ["add", "sample.rs"])?;
        run_git(&repository, [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "test fixture",
        ])?;
        run_git(&repository, [
            "worktree",
            "add",
            "-b",
            "test-worktree",
            worktree.to_str().context("non-UTF-8 path")?,
        ])?;

        let fingerprint = git_workspace_fingerprint(&worktree)?;
        let state = build_git_index_state(&worktree, None, fingerprint)?;

        assert_eq!(state.files.len(), 1);
        assert!(state.files.contains_key(Path::new("sample.rs")));
        assert!(git_workspace_fingerprint(&worktree)?.is_some());
        Ok(())
    }

    #[test]
    fn git_fingerprint_is_disabled_while_the_workspace_is_dirty() -> Result<()> {
        let directory = tempfile::tempdir()?;
        run_git(directory.path(), ["init", "--initial-branch=main"])?;
        let source = directory.path().join("sample.rs");
        std::fs::write(&source, "pub struct Initial;\n")?;
        run_git(directory.path(), ["add", "sample.rs"])?;
        run_git(directory.path(), [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "test fixture",
        ])?;

        let clean = git_workspace_fingerprint(directory.path())?.context("missing fingerprint")?;
        std::fs::write(&source, "pub struct Changed;\n")?;

        assert!(!clean.is_empty());
        assert!(git_workspace_fingerprint(directory.path())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn refreshes_the_index_when_sparse_checkout_rules_change() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("repository");
        std::fs::create_dir(&workspace)?;
        init_git_workspace(&workspace)?;
        std::fs::create_dir(workspace.join("included"))?;
        std::fs::create_dir(workspace.join("excluded"))?;
        std::fs::write(
            workspace.join("included/included.rs"),
            "pub struct IncludedSymbol;\n",
        )?;
        std::fs::write(
            workspace.join("excluded/excluded.rs"),
            "pub struct ExcludedSymbol;\n",
        )?;
        run_git(&workspace, ["add", "."])?;
        run_git(&workspace, [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "test fixture",
        ])?;
        run_git(&workspace, ["sparse-checkout", "init", "--cone"])?;
        run_git(&workspace, [
            "sparse-checkout",
            "set",
            "included",
            "excluded",
        ])?;
        let search = OnomaSearch {
            storage: directory.path().join("cache"),
            workspace: workspace.clone(),
        };

        assert_eq!(search.find("ExcludedSymbol").await?.len(), 1);
        run_git(&workspace, ["sparse-checkout", "set", "included"])?;

        assert!(
            search
                .find("ExcludedSymbol")
                .await?
                .iter()
                .all(|symbol| symbol.name != "ExcludedSymbol")
        );
        assert_eq!(search.find("IncludedSymbol").await?.len(), 1);
        Ok(())
    }

    #[test]
    fn automatic_gc_waits_for_the_grace_period() -> Result<()> {
        let storage = tempfile::tempdir()?;
        let missing_workspace = storage.path().join("missing");
        let record = storage.path().join("test.workspace");
        write_workspace_record(&record, &WorkspaceRecord {
            path: missing_workspace,
            missing_since: None,
        })?;
        std::fs::write(storage.path().join("test.db"), [])?;
        let first_check = UNIX_EPOCH + Duration::from_secs(1_000_000);

        assert_eq!(gc_cache(storage.path(), false, first_check)?, 0);
        assert!(storage.path().join("test.db").exists());
        assert_eq!(
            gc_cache(
                storage.path(),
                false,
                first_check + Duration::from_secs(GC_GRACE_PERIOD_SECS),
            )?,
            1
        );
        assert!(!storage.path().join("test.db").exists());
        assert!(!record.exists());
        Ok(())
    }

    #[test]
    fn forced_gc_removes_a_missing_workspace_immediately() -> Result<()> {
        let storage = tempfile::tempdir()?;
        let record = storage.path().join("test.workspace");
        write_workspace_record(&record, &WorkspaceRecord {
            path: storage.path().join("missing"),
            missing_since: None,
        })?;
        std::fs::write(storage.path().join("test.db"), [])?;

        assert_eq!(gc_cache(storage.path(), true, SystemTime::now())?, 1);
        assert!(!storage.path().join("test.db").exists());
        assert!(!record.exists());
        Ok(())
    }

    fn run_git<const N: usize>(directory: &Path, args: [&str; N]) -> Result<()> {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(directory)
            .status()?;
        anyhow::ensure!(status.success(), "git failed with {status}");
        Ok(())
    }

    fn init_git_workspace(directory: &Path) -> Result<()> {
        run_git(directory, ["init", "--initial-branch=main"])
    }

    #[tokio::test]
    async fn keeps_duplicate_exact_declarations_for_the_picker() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        init_git_workspace(workspace.path())?;
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
        init_git_workspace(workspace.path())?;
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
    async fn deleting_a_file_does_not_deindex_a_file_with_the_same_prefix() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        init_git_workspace(workspace.path())?;
        let removed = workspace.path().join("foo.js");
        let retained = workspace.path().join("foo.jsx");
        std::fs::write(&removed, "function RemovedSymbol() {}\n")?;
        std::fs::write(&retained, "function RetainedSymbol() {}\n")?;
        let search = OnomaSearch {
            storage: workspace.path().join("cache"),
            workspace: workspace.path().to_path_buf(),
        };

        assert_eq!(search.find("RemovedSymbol").await?.len(), 1);
        assert_eq!(search.find("RetainedSymbol").await?.len(), 1);
        std::fs::remove_file(removed)?;

        assert!(
            search
                .find("RemovedSymbol")
                .await?
                .iter()
                .all(|symbol| symbol.name != "RemovedSymbol")
        );
        let results = search.find("RetainedSymbol").await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, retained);
        Ok(())
    }

    #[tokio::test]
    async fn removes_symbols_that_become_gitignored() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        init_git_workspace(workspace.path())?;
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
