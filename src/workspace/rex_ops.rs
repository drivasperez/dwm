use anyhow::Result;
use owo_colors::OwoColorize;
use std::path::{Path, PathBuf};

use crate::rex::{self, RexApi, Shown};
use crate::workspace::layout;

/// Explain why Rex didn't switch sessions. The usual cause is that the Rex
/// app doesn't take actions from scripts until Remote Control is on.
fn warn_not_shown(reason: &str) {
    eprintln!(
        "{} Rex won't switch sessions for dwm, so changing directory instead. Turn on Remote Control in Rex's Settings > General, or pass --here to skip Rex.",
        "note:".yellow()
    );
    eprintln!("  {}", reason.dimmed());
}

/// The Rex session label for a workspace path: the workspace's name, or the
/// repo directory's name for the main workspace.
pub(crate) fn session_label_for(path: &Path) -> String {
    if let Some((_, name, _)) = layout::find_dwm_workspace(path) {
        return name.to_string();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Take the person into a workspace that was just created. Inside Rex that
/// means a new session for it, running `command` if one was given. Returns
/// where the shell should `cd` when no session was shown.
pub(crate) fn open_new_workspace(
    rex: Option<&dyn RexApi>,
    ws_name: &str,
    ws_path: &Path,
    placement: super::Placement,
    command: &[String],
) -> Result<Option<PathBuf>> {
    let cd = Some(ws_path.to_path_buf());
    let Some(rex) = rex else { return Ok(cd) };
    if placement == super::Placement::Here {
        return Ok(cd);
    }
    let initial_input =
        (!command.is_empty()).then(|| format!("{}\r", rex::shell_command_line(command)));
    let req = rex::OpenSession {
        label: ws_name.to_string(),
        cwd: ws_path.to_path_buf(),
        create: if initial_input.is_some() {
            rex::CreateWhen::Always
        } else {
            rex::CreateWhen::OnlyIfShown
        },
        initial_input,
    };
    let opened = match rex.open_session(&req) {
        Ok(o) => o,
        Err(e) if command.is_empty() => {
            eprintln!(
                "{} could not open a Rex session: {e:#}",
                "warning:".yellow()
            );
            return Ok(cd);
        }
        Err(e) => return Err(e.context("could not open a Rex session for the command")),
    };
    match opened.shown {
        Shown::Yes => {
            eprintln!("{} opened Rex session '{}'", "→".cyan(), ws_name.bold());
            Ok(None)
        }
        Shown::No(reason) => {
            if opened.created {
                eprintln!(
                    "{} started Rex session '{}', but Rex didn't switch to it",
                    "→".cyan(),
                    ws_name.bold()
                );
            }
            warn_not_shown(&reason);
            Ok(cd)
        }
    }
}

/// Take the person to an existing workspace. Inside Rex, focus a pane already
/// in it, or open a session for it. Returns where the shell should `cd` when
/// neither happened, including when the current pane is already there.
pub(crate) fn go_to_workspace(
    rex: Option<&dyn RexApi>,
    cwd: &Path,
    ws_path: &Path,
    placement: super::Placement,
) -> Result<Option<PathBuf>> {
    let cd = Some(ws_path.to_path_buf());
    let Some(rex) = rex else { return Ok(cd) };
    if placement == super::Placement::Here {
        return Ok(cd);
    }
    // Already there: just cd, unless this pane is about to close anyway.
    if rex.current_block().is_some()
        && rex.current_pane() == rex::CurrentPane::Lasting
        && layout::is_inside(&rex::real_path(cwd), &rex::real_path(ws_path))
    {
        return Ok(cd);
    }
    let panes = match rex.panes() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{} could not list Rex panes: {e:#}", "warning:".yellow());
            return Ok(cd);
        }
    };
    let inside = rex::panes_inside(&panes, ws_path, rex.current_block());
    if let Some(pane) = rex::pick_pane(&inside) {
        return match rex.focus(pane) {
            Ok(Shown::Yes) => {
                eprintln!("{} switched to Rex {}", "→".cyan(), pane.describe());
                Ok(None)
            }
            Ok(Shown::No(reason)) => {
                warn_not_shown(&reason);
                Ok(cd)
            }
            Err(e) => {
                eprintln!(
                    "{} could not focus the Rex pane: {e:#}",
                    "warning:".yellow()
                );
                Ok(cd)
            }
        };
    }
    let label = session_label_for(ws_path);
    let req = rex::OpenSession {
        label: label.clone(),
        cwd: ws_path.to_path_buf(),
        initial_input: None,
        create: rex::CreateWhen::OnlyIfShown,
    };
    match rex.open_session(&req) {
        Ok(rex::Opened {
            shown: Shown::Yes, ..
        }) => {
            eprintln!("{} opened Rex session '{}'", "→".cyan(), label.bold());
            Ok(None)
        }
        Ok(rex::Opened {
            shown: Shown::No(reason),
            ..
        }) => {
            warn_not_shown(&reason);
            Ok(cd)
        }
        Err(e) => {
            eprintln!(
                "{} could not open a Rex session: {e:#}",
                "warning:".yellow()
            );
            Ok(cd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rex::ProgramState::Idle;
    use crate::rex::tests::{FakeRex, pane};
    use crate::workspace::test_utils::{make_repo_with_vcs, ws};
    use crate::workspace::types::Placement;
    use std::fs;

    #[test]
    fn switch_focuses_a_pane_already_in_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(a.join("src")).unwrap();
        let fake = FakeRex::new(
            vec![
                pane("shell", "a", &a.join("src"), &[]),
                pane("agent", "a", &a, &[Idle]),
            ],
            true,
        )
        .in_block("me");

        let cd = go_to_workspace(Some(&fake), &main_repo, &a, Placement::Rex).unwrap();
        assert_eq!(cd, None, "focusing a pane leaves the shell where it is");
        assert_eq!(fake.calls(), vec!["panes", "focus agent"]);
    }

    #[test]
    fn switch_falls_back_to_cd_when_rex_wont_show_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(vec![pane("p", "a", &a, &[])], false).in_block("me");

        let cd = go_to_workspace(Some(&fake), &main_repo, &a, Placement::Rex).unwrap();
        assert_eq!(cd, Some(a));
        assert_eq!(fake.calls(), vec!["panes", "focus p"]);
    }

    #[test]
    fn switch_opens_a_session_named_after_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        // A pane in the main checkout doesn't count for workspace `a`.
        let fake = FakeRex::new(vec![pane("p", "repo", &main_repo, &[])], true).in_block("me");

        let cd = go_to_workspace(Some(&fake), &main_repo, &a, Placement::Rex).unwrap();
        assert_eq!(cd, None);
        let opened = fake.opened.borrow();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].label, "a");
        assert_eq!(opened[0].cwd, a);
        assert_eq!(opened[0].create, crate::rex::CreateWhen::OnlyIfShown);
        assert_eq!(opened[0].initial_input, None);
    }

    #[test]
    fn switch_to_main_labels_the_session_after_the_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(vec![], true).in_block("me");

        go_to_workspace(Some(&fake), &a, &main_repo, Placement::Rex).unwrap();
        assert_eq!(fake.opened.borrow()[0].label, "myrepo");
    }

    #[test]
    fn switch_changes_directory_when_this_pane_is_already_there() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(a.join("src")).unwrap();
        let fake = FakeRex::new(vec![pane("other", "a", &a, &[])], true).in_block("me");

        let cd = go_to_workspace(Some(&fake), &a.join("src"), &a, Placement::Rex).unwrap();
        assert_eq!(cd, Some(a));
        assert!(fake.calls().is_empty(), "no need to ask Rex anything");
    }

    #[test]
    fn switch_from_a_throwaway_pane_uses_rex_even_when_already_there() {
        // The palette's floating picker runs in the repo it lists, and
        // closes when dwm exits, so a cd there would do nothing visible.
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let mut fake =
            FakeRex::new(vec![pane("me", "float", &main_repo, &[])], true).in_block("me");
        fake.pane = crate::rex::CurrentPane::Throwaway;

        let cd = go_to_workspace(Some(&fake), &main_repo, &main_repo, Placement::Rex).unwrap();
        assert_eq!(cd, None);
        // The picker's own pane is never the one focused.
        assert_eq!(fake.calls(), vec!["panes", "open myrepo"]);
    }

    #[test]
    fn switch_here_and_outside_rex_change_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let main_repo = make_repo_with_vcs(tmp.path());
        let a = ws(&main_repo, "a");
        fs::create_dir_all(&a).unwrap();
        let fake = FakeRex::new(vec![pane("p", "a", &a, &[])], true);

        let cd = go_to_workspace(Some(&fake), &main_repo, &a, Placement::Here).unwrap();
        assert_eq!(cd, Some(a.clone()));
        assert!(fake.calls().is_empty());
        let cd = go_to_workspace(None, &main_repo, &a, Placement::Rex).unwrap();
        assert_eq!(cd, Some(a));
    }
}
