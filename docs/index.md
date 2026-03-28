# jj-spice

Submit, sync, and track stacked change requests in your [jj (Jujutsu)](https://github.com/jj-vcs/jj) repository without the busywork.

Unlike a wrapper, `jj-spice` is a complement to `jj` and built directly on [jj-lib](https://crates.io/crates/jj-lib) for deep, native integration.

Stacked change requests break a large change into a chain of small, reviewable
PRs that depend on each other. `jj-spice` automates the tedious parts — creating
the PRs, keeping their base branches in sync, and tracking their status.

`jj-spice` allows you to:
- Submit a stack of change requests
- Sync the current stack with a remote repository
- Visualize the stack and its review status

The following version control systems are supported:
- [GitHub](https://github.com)
