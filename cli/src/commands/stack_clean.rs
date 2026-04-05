//! `jj-spice stack clean` — remove stale and inactive CR entries.

use std::collections::HashSet;
use std::io::Write as _;

use jj_spice_lib::clean::{apply_clean, identify_cleanable};
use jj_spice_lib::forge::Forge;
use jj_spice_lib::store::change_request::ChangeRequestStore;

use crate::commands::cli::CleanArgs;
use crate::commands::env::SpiceEnv;

/// Remove stale and inactive change request entries from local tracking.
///
/// Stale entries are bookmarks that no longer exist in the repository.
/// Inactive entries are change requests that are closed or merged on the forge.
pub async fn run(
    args: &CleanArgs,
    env: &SpiceEnv,
    forge: &dyn Forge,
) -> Result<(), Box<dyn std::error::Error>> {
    let cr_store = ChangeRequestStore::new(&env.store);
    let state = cr_store.load()?;

    if state.is_empty() {
        writeln!(env.ui.status(), "No tracked change requests")?;
        return Ok(());
    }

    // Collect local bookmark names from the repo view.
    let local_bookmarks: HashSet<String> = env
        .repo
        .view()
        .bookmarks()
        .map(|(name, _)| name.as_str().to_string())
        .collect();

    let result = identify_cleanable(&state, &local_bookmarks, forge).await;

    if result.total() == 0 {
        writeln!(env.ui.status(), "Nothing to clean")?;
        return Ok(());
    }

    for entry in &result.entries {
        let prefix = if args.dry_run {
            "Would remove"
        } else {
            "Removing"
        };
        writeln!(
            env.ui.status(),
            "{prefix} {}: {} ({})",
            entry.bookmark,
            entry.meta,
            entry.reason,
        )?;
    }

    if args.dry_run {
        writeln!(
            env.ui.status(),
            "Dry run: {} entry(ies) would be removed ({} stale, {} inactive)",
            result.total(),
            result.stale_count(),
            result.inactive_count(),
        )?;
        return Ok(());
    }

    let mut state = state;
    apply_clean(&mut state, &result);
    cr_store.save(&state)?;

    writeln!(
        env.ui.status(),
        "Cleaned {} entry(ies) ({} stale, {} inactive)",
        result.total(),
        result.stale_count(),
        result.inactive_count(),
    )?;

    Ok(())
}
