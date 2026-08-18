use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
#[cfg(not(unix))]
use anyhow::bail;
use onoma::models::resolved::ResolvedSymbol;

pub(crate) struct Neovim {
    executable: PathBuf,
    workspace: PathBuf,
}

impl Neovim {
    pub(crate) fn new(executable: PathBuf, workspace: PathBuf) -> Self {
        Self {
            executable,
            workspace,
        }
    }

    pub(crate) fn open_location(&self, symbol: &ResolvedSymbol) -> Result<()> {
        let command = format!(
            "call cursor({}, {})",
            symbol.start_line, symbol.start_column
        );
        self.launch([
            symbol.path.as_os_str().to_owned(),
            OsString::from("-c"),
            OsString::from(command),
        ])
    }

    pub(crate) fn open_search(&self, query: &str) -> Result<()> {
        let query = serde_json::to_string(query)?;
        let query = lua_long_string(&query);
        let lua = format!(
            "local q=vim.json.decode({query});vim.schedule(function() local ok,s=pcall(require,'snacks');if ok and s.picker then s.picker.grep({{search=q}});return end;local ok,t=pcall(require,'telescope.builtin');if ok then t.live_grep({{default_text=q}});return end;vim.fn.setreg('/',vim.pesc(q));vim.cmd('silent! grep! '..vim.fn.shellescape(q));vim.cmd.copen() end)"
        );
        self.launch([OsString::from("-c"), OsString::from(format!("lua {lua}"))])
    }

    fn launch<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.workspace)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        #[cfg(unix)]
        {
            let error = command.exec();
            Err(error).with_context(|| format!("cannot start {}", self.executable.display()))
        }

        #[cfg(not(unix))]
        {
            let status = command
                .status()
                .with_context(|| format!("cannot start {}", self.executable.display()))?;
            if !status.success() {
                bail!("Neovim exited with {status}");
            }
            Ok(())
        }
    }
}

fn lua_long_string(value: &str) -> String {
    // Always use at least one equals sign. JSON arrays end with `]`, so the
    // empty long-bracket delimiter would turn the boundary into `]]]` and
    // leave one closing bracket outside the Lua string.
    let mut equals = String::from("=");
    while value.contains(&format!("]{equals}]")) {
        equals.push('=');
    }
    format!("[{equals}[{value}]{equals}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_bracket_string_preserves_json_quotes() {
        assert_eq!(
            lua_long_string(r#"[{"name":"thing"}]"#),
            r#"[=[[{"name":"thing"}]]=]"#
        );
    }

    #[test]
    fn long_bracket_string_avoids_embedded_delimiter() {
        assert_eq!(lua_long_string("a]=]b"), "[==[a]=]b]==]");
    }

    #[test]
    fn workspace_is_kept_by_editor() {
        use std::path::Path;

        let editor = Neovim::new(PathBuf::from("nvim"), PathBuf::from("/tmp/project"));
        assert_eq!(editor.workspace, Path::new("/tmp/project"));
    }
}
