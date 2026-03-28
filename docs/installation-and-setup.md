# Installation and setup

## Installation

See below on how to install on your machine. There is also [pre-built binaries](https://github.com/alejoborbo/jj-spice/releases) for Windows, Linux, and MacOS.

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

## Use it directly in jj

You can register `jj-spice` an alias, and use it directly with `jj`. It can be setup using the following subcommand:

```bash
jj-spice util install-aliases
```

After installation, the following shortcuts are available:

- `jj stack <cmd>` instead of `jj-spice stack <cmd>`
- `jj spice <cmd>` instead of `jj-spice <cmd>`

## Shell completion

`jj-spice` supports two completion methods: **dynamic** (recommended) and
**static**.

Dynamic completion calls back into `jj-spice` at TAB-time, so completions
stay in sync with the installed version and can complete config keys and
values. Static completion generates a one-time script that only knows about
subcommands and flags.

### Dynamic completion (recommended)

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
