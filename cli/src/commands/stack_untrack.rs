//! `jj-spice stack untrack` — stop tracking CRs for specific bookmarks.

use std::collections::HashSet;
use std::io::Write as _;

use jj_spice_lib::clean::{CleanResult, apply_clean, find_inactive_entries};
use jj_spice_lib::forge::Forge;
use jj_spice_lib::store::change_request::ChangeRequestStore;

use crate::commands::cli::UntrackArgs;
use crate::commands::env::SpiceEnv;

/// Remove bookmark-to-CR mappings from local storage.
///
/// When `--all-inactive` is passed, queries the forge to find all tracked
/// entries whose CR is closed (always) or merged with no local bookmark,
/// and removes them. Otherwise removes only the explicitly named bookmarks.
pub async fn run(
    args: &UntrackArgs,
    env: &SpiceEnv,
    forge: Option<&dyn Forge>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cr_store = ChangeRequestStore::new(&env.store);
    let mut state = cr_store.load()?;
    let initial_count = state.len();

    if args.all_inactive {
        let forge = forge
            .ok_or("no forge detected — --all-inactive requires a forge to check CR status")?;

        let local_bookmarks: HashSet<String> = env
            .repo
            .view()
            .bookmarks()
            .map(|(name, _)| name.as_str().to_string())
            .collect();

        let inactive = find_inactive_entries(&state, forge, &local_bookmarks).await;
        let result = CleanResult { entries: inactive };

        if result.total() == 0 {
            writeln!(env.ui.status(), "No inactive change requests found")?;
            return Ok(());
        }

        for entry in &result.entries {
            writeln!(
                env.ui.status(),
                "Untracking {}: {} ({})",
                entry.bookmark,
                entry.meta,
                entry.reason,
            )?;
        }

        apply_clean(&mut state, &result);
    }

    // Remove explicitly named bookmarks.
    for name in &args.bookmarks {
        if state.remove(name) {
            writeln!(env.ui.status(), "Untracked: {name}")?;
        } else {
            writeln!(env.ui.warning_default(), "{name}: not tracked, skipping")?;
        }
    }

    let removed = initial_count - state.len();
    if removed > 0 {
        cr_store.save(&state)?;
        writeln!(
            env.ui.status(),
            "Removed {removed} entry(ies) from tracking"
        )?;
    }

    Ok(())
}
