# Configuration reference

`jj-spice` is configured through the standard jj configuration stack. All
settings live under the `[spice]` table and can be set at any level of the
hierarchy (user, repo, or workspace) using the regular jj config files or the
`--config` CLI flag.

For details on how jj resolves its configuration, see the
[jj configuration docs](https://jj-vcs.github.io/jj/latest/config/).

## Quick reference

| Key | Type | Default | Related CLI flag |
|-----|------|---------|-----------------|
| [`spice.output`](#spiceoutput) | `"modern"` \| `"classic"` | `"modern"` | -- |
| [`spice.auto-accept-changes`](#spiceauto-accept-changes) | `bool` | `false` | `--auto-accept` |
| [`spice.auto-clean`](#spiceauto-clean) | `bool` | `true` | -- |
| [`spice.upstream-remote`](#spiceupstream-remote) | `string` | auto-detected | -- |
| [`spice.forges.<hostname>.type`](#spiceforgeshostnametype) | `"github"` \| `"gitlab"` \| `"gitea"` | auto-detected | -- |

## `spice.output`

Terminal output fidelity.

- **`"modern"`** (default) -- Nerd Font Powerline glyphs for status badges,
  OSC 8 terminal hyperlinks. Requires a
  [Nerd Font](https://www.nerdfonts.com/).
- **`"classic"`** -- ASCII brackets `[Status]` with foreground-only colors,
  plain-text URL fallbacks. Safe for any terminal.

```toml
[spice]
output = "classic"
```

**Affects:** `stack log`, `stack submit`, `stack sync`.

## `spice.auto-accept-changes`

When `true`, untracked changes are pushed automatically during `stack submit`
without prompting.

When unset or `false`, the user is prompted before pushing.

Equivalent to passing `--auto-accept` on the command line. The config value
takes precedence over the flag when set.

```toml
[spice]
auto-accept-changes = true
```

**Affects:** `stack submit`.

## `spice.auto-clean`

When `true` (the default), inactive change requests (closed or merged on the
forge) are automatically removed from local tracking during `stack submit` and
`stack sync`.

Set to `false` to keep inactive entries until they are removed manually with
`stack clean` or `stack untrack`.

```toml
[spice]
auto-clean = false
```

**Affects:** `stack submit`, `stack sync`.

## `spice.upstream-remote`

Override the upstream remote name used for fork workflows.

In a fork setup, `jj-spice` pushes branches to the *push remote*
(`git.push`, defaulting to `"origin"`) and creates change requests against
the *upstream remote*. By default, a remote named `"upstream"` is used when
it exists. Set this key to use a different name.

```toml
[spice]
upstream-remote = "upstream"
```

**Affects:** all `stack` subcommands.

## `spice.forges.<hostname>.type`

Register a custom hostname as a specific forge type.

`jj-spice` automatically recognises `github.com`, `gitlab.com`, and `gitea.com`. For
self-hosted instances (GitHub Enterprise, self-managed GitLab, etc.),
map the hostname to a forge type so that `jj-spice` can interact with its
API.

This setting is typically written to the *repo-level* config by `stack sync`
when it encounters an unknown hostname and prompts the user to select a forge
type. It can also be set manually.

```toml
[spice.forges."git.corp.example.com"]
type = "github"
```

Accepted values: `"github"`, `"gitlab"`, `"gitea"`.

For a self-hosted Gitea server, set `type = "gitea"` under its hostname.
`jj-spice` uses HTTPS to access the API at the root of that host. Set
`GITEA_TOKEN` to a personal access token. For `stack submit`, the token
needs write access to repositories and issues so it can create pull requests
and add stack comments. Draft submission uses Gitea's default `WIP:` prefix.

**Affects:** all `stack` subcommands.
