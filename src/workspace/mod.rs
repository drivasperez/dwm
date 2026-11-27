//! Workspace management: creation, deletion, switching, renaming, pruning,
//! listing, and status output for both jj and git repositories.
//!
//! This module is split into submodules by concern:
//! - [`layout`] — directory layout, path helpers, workspace detection
//! - [`registry`] — cross-repo registry for `dwm list --all`
//! - [`types`] — shared enums, structs, and helpers
//! - [`rex_ops`] — Rex terminal integration (session open/focus)
//! - [`new`] — workspace creation
//! - [`switch`] — workspace switching
//! - [`delete`] — workspace deletion (including open-pane handling)
//! - [`rename`] — workspace renaming
//! - [`lease`] — pool-based workspace leasing
//! - [`prune`] — stale workspace pruning
//! - [`list`] — workspace enumeration
//! - [`status`] — table and JSON status output

mod delete;
mod layout;
mod lease;
mod list;
mod new;
mod prune;
mod registry;
mod rename;
mod rex_ops;
mod status;
mod switch;
mod types;

#[cfg(test)]
mod test_utils;

// Re-export the public API at `crate::workspace::*` so callers (main.rs,
// tui.rs, cli.rs) are unchanged.
pub use delete::delete_workspace;
pub(crate) use layout::dwm_dir;
pub use layout::find_dwm_workspace;
#[cfg(test)]
pub(crate) use layout::worktrees_dir;
pub use lease::{lease_workspace, release_workspace};
pub use list::{
    list_all_workspace_entries, list_workspace_entries, list_workspace_entries_and_dwm_dir,
};
pub use new::new_workspace;
pub use prune::prune_workspaces;
pub use rename::rename_workspace;
pub use status::{print_status, print_status_json};
pub use switch::{switch_to_path, switch_workspace};
pub use types::{
    DeleteOutput, OpenPanes, Placement, PruneMode, Terminal, WorkspaceEntry, effective_placement,
    format_time_ago,
};
