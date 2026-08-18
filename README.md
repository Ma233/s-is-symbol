# s-is-symbol

[![Check and test](https://github.com/ma233/s-is-symbol/actions/workflows/check_and_test.yml/badge.svg)](https://github.com/ma233/s-is-symbol/actions/workflows/check_and_test.yml)
[![Build](https://github.com/ma233/s-is-symbol/actions/workflows/build.yml/badge.svg)](https://github.com/ma233/s-is-symbol/actions/workflows/build.yml)

`s` is for symbol. `s-is-symbol` uses [Onoma](https://github.com/ryanmab/onoma) to find code declarations and open them in Neovim.
The workspace must be inside a Git working tree; regular checkouts and Git worktrees are supported.

## Installation

Download and run the installer from the latest public GitHub Release. The installer requires `curl` and `unzip`:

```sh
curl -fsSL https://github.com/ma233/s-is-symbol/releases/latest/download/install.sh | sh
```

To install the rolling prerelease build:

```sh
curl -fsSL https://github.com/ma233/s-is-symbol/releases/latest/download/install.sh | SYMBOL_VERSION=prerelease sh
```

You can also build and install directly from the source tree:

```sh
cargo install --path .
```

Indexes for deleted projects and Git worktrees are cleaned automatically after
a seven-day grace period. Run `s --gc` to remove them immediately.

## Usage

```sh
s UserService
s create_user ./backend
```

## Behavior

- One exact match: opens Neovim and jumps to the declaration.
- Multiple matches or no exact match: opens a prefilled Snacks grep picker, falls back to Telescope `live_grep`, and finally falls back to the Neovim quickfix list.

Use `--nvim` or `SYMBOL_NVIM` to select the Neovim executable:

```sh
s UserService --nvim /opt/homebrew/bin/nvim
```

Onoma indexes are stored in `$XDG_CACHE_HOME/symbol/onoma`. If `XDG_CACHE_HOME` is not set, Symbol uses `$HOME/.cache/symbol/onoma`.

Onoma currently supports Rust, Go, Lua, Clojure, TypeScript, JavaScript, and Python.

## Development

Install the Git hooks to run file formatting, dependency license, spelling, compilation, and Clippy checks before each commit:

```sh
pre-commit install
pre-commit run --all-files
```

The primary local verification commands are:

```sh
cargo +nightly fmt --check
cargo clippy --workspace --all-targets --all-features --locked -- --deny warnings
cargo test --workspace --all-features --locked
cargo deny check
```

GitHub Actions applies the same quality gates to pull requests and the `main` branch. It also produces release artifacts for macOS ARM64, Linux x86-64, and Linux ARM64.

## License

`s-is-symbol` is distributed under the [MIT License](LICENSE).
