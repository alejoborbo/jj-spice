# jj-spice

`jj-spice` manages stacked change requests in a [jj (Jujutsu)](https://github.com/jj-vcs/jj) repository.

Stacked change requests break a large change into a chain of small, reviewable
PRs that depend on each other. `jj-spice` automates the tedious parts — creating
the PRs, keeping their base branches in sync, and tracking their status.

`jj-spice` allows you to:
- Submit a stack of change requests
- Sync the current stack with a remote repository
- Visualize the stack and its review status

The following version control systems are supported:
- [GitHub](https://github.com)

## Demo

[![asciicast](https://asciinema.org/a/kBv6aeMHxa0KaMt3.svg)](https://asciinema.org/a/kBv6aeMHxa0KaMt3)

## Prerequisites

- [GitHub CLI (`gh`)](https://cli.github.com) — required if your repository is hosted on GitHub. Must be authenticated (`gh auth login`)

## Installation

### Homebrew (macOS)

```bash
brew install alejoborbo/tap/jj-spice
```

### Scoop (Windows)

```powershell
scoop install alejoborbo_scoop-bucket/jj-spice
```

### Winget (Windows)

```powershell
winget install alejoborbo.jj-spice
```

### Cargo

```bash
cargo install jj-spice-cli
```

### From source

```bash
git clone https://github.com/alejoborbo/jj-spice.git
cd jj-spice
cargo install --path cli
```

### Direct download

Pre-built binaries for Linux, macOS, and Windows are available on the
[GitHub Releases](https://github.com/alejoborbo/jj-spice/releases) page.

## Usage

### Alias to use jj command directly

Register jj aliases so that `jj-spice` commands can be invoked directly as `jj`
subcommands.

```bash
jj-spice util install-aliases
```

After installation, the following shortcuts are available:

- `jj stack <cmd>` instead of `jj-spice stack <cmd>`
- `jj spice <cmd>` instead of `jj-spice <cmd>`

### `jj-spice stack log`

Visualize the bookmark DAG with the status of each change request.

```bash
# Show the full stack
jj-spice stack log

# Filter to a specific revset
jj-spice stack log -r 'trunk()..@'
```

### `jj-spice stack submit`

Create or update change requests for every bookmark in the stack. Prompts
interactively for the title, description, and draft status of new PRs.

```bash
jj-spice stack submit
```

### `jj-spice stack sync`

Discover existing change requests on the remote and start tracking them locally.

```bash
# Sync untracked bookmarks
jj-spice stack sync

# Re-sync already-tracked bookmarks
jj-spice stack sync --force
```

## Shell completion

`jj-spice` supports two completion methods: **dynamic** (recommended) and
**static**.

Dynamic completion calls back into `jj-spice` at TAB-time, so completions
stay in sync with the installed version and can complete config keys and
values. Static completion generates a one-time script that only knows about
subcommands and flags.

<details>
<summary><strong>Dynamic completion (recommended)</strong></summary>

Add one of the following to your shell startup file:

**Bash** (`~/.bashrc`):

```bash
source <(COMPLETE=bash jj-spice)
```

**Zsh** (`~/.zshrc`):

```zsh
source <(COMPLETE=zsh jj-spice)
```

**Fish** (`~/.config/fish/config.fish`):

```fish
COMPLETE=fish jj-spice | source
```

**Elvish**:

```elvish
eval (COMPLETE=elvish jj-spice | slurp)
```

**PowerShell**:

```powershell
COMPLETE=powershell jj-spice | Out-String | Invoke-Expression
```

</details>

<details>
<summary><strong>Static completion</strong></summary>

Generate a static script with `jj-spice util completion <SHELL>`. This is
useful when dynamic completion is not available (e.g. Nushell) or when you
prefer a pre-generated script.

**Bash**:

```bash
jj-spice util completion bash > ~/.local/share/bash-completion/completions/jj-spice
```

**Zsh** (write to a directory in your `$fpath`):

```zsh
jj-spice util completion zsh > ~/.zfunc/_jj-spice
```

**Fish**:

```fish
jj-spice util completion fish > ~/.config/fish/completions/jj-spice.fish
```

**Nushell** (static only — dynamic completion is not supported):

```nu
jj-spice util completion nushell | save -f "completions-jj-spice.nu"
use "completions-jj-spice.nu" *
```

**PowerShell**:

```powershell
jj-spice util completion power-shell | Out-String | Invoke-Expression
```

**Elvish**:

```elvish
eval (jj-spice util completion elvish | slurp)
```

</details>

## License

Licensed under Apache 2.0 — see [LICENSE](LICENSE).
