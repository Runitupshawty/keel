//! WSL (Task 51). On Windows: the sidebar's WSL section lists the distributions
//! (`keel_term::wsl_distros`, the terminal's list too) and opens one as
//! `\\wsl.localhost\<name>` (else `\\wsl$\<name>`) through the local provider, like any
//! network share. There is no Recycle Bin there, so a delete is permanent and the prompt
//! says so.

use keel_vfs::{Provider, VPath};
use std::time::Duration;

/// How long opening a distribution may take (a stopped one starts first) before the
/// sidebar gives up with an error toast.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// The names Windows serves distributions under: `wsl.localhost` (Windows 11, Windows 10
/// 21H2 and later), then the older `wsl$`.
const HOSTS: [&str; 2] = ["wsl.localhost", "wsl$"];

/// Opens distribution `name`: lists its root through the local provider (which starts a
/// stopped distribution) and returns that root. Blocks for as long as starting takes:
/// workers only (`worker::spawn_wsl`, with `OPEN_TIMEOUT`).
pub fn open(name: &str) -> anyhow::Result<VPath> {
    let mut failed = Vec::new();
    for host in HOSTS {
        let root = VPath::local(format!(r"\\{host}\{name}"));
        match keel_vfs::LocalProvider.list(&root) {
            Ok(_) => return Ok(root),
            Err(e) => failed.push(format!("{}: {e:#}", root.display())),
        }
    }
    anyhow::bail!("{}", failed.join("; "))
}

/// The distribution a local path is in (`\\wsl.localhost\<name>\...` or
/// `\\wsl$\<name>\...`), None elsewhere. Pure: never touches the disk.
pub fn distro_of(p: &VPath) -> Option<String> {
    let path = p.to_local_path()?.to_string_lossy().replace('\\', "/");
    let rest = HOSTS.iter().find_map(|host| {
        let prefix = format!("//{host}/");
        path.get(..prefix.len())
            .filter(|head| head.eq_ignore_ascii_case(&prefix))
            .map(|_| &path[prefix.len()..])
    })?;
    let name = rest.split('/').next().unwrap_or_default();
    (!name.is_empty()).then(|| name.to_owned())
}

/// The distribution all of `paths` are in, if they are in one. Such a delete is permanent
/// (`jobs::delete`) after `delete_text`; a mix with other places goes the trash's way,
/// which refuses the WSL items.
pub fn one_distro(paths: &[VPath]) -> Option<String> {
    let first = distro_of(paths.first()?)?;
    paths
        .iter()
        .all(|p| distro_of(p).as_ref() == Some(&first))
        .then_some(first)
}

/// The prompt before deleting `n` items in distribution `distro` (no Recycle Bin there).
pub fn delete_text(n: usize, distro: &str) -> String {
    format!(
        "Delete {} in {distro} (WSL) permanently? WSL folders have no Recycle Bin; this cannot be undone.",
        crate::jobs::items(n)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_paths_are_recognised() {
        let d = |p: &str| distro_of(&VPath::local(p));
        if cfg!(windows) {
            assert_eq!(
                d(r"\\wsl.localhost\Ubuntu\home\x").as_deref(),
                Some("Ubuntu")
            );
            assert_eq!(d(r"\\WSL.LOCALHOST\Ubuntu").as_deref(), Some("Ubuntu"));
            assert_eq!(d(r"\\wsl$\Debian\etc").as_deref(), Some("Debian"));
            assert_eq!(d(r"\\?\UNC\wsl$\Debian\etc").as_deref(), Some("Debian"));
            assert_eq!(d(r"\\server\share\x"), None);
            assert_eq!(d(r"\\wsl.localhost\"), None);
            assert_eq!(d(r"C:\wsl.localhost\x"), None);
        }
        assert_eq!(
            distro_of(&VPath::parse("sftp://wsl.localhost/x").unwrap()),
            None
        );
        let v = |p: &str| VPath::local(p);
        if cfg!(windows) {
            let u = [v(r"\\wsl.localhost\U\a"), v(r"\\wsl$\U\b")];
            assert_eq!(one_distro(&u).as_deref(), Some("U"));
            let mixed = [v(r"\\wsl.localhost\U\a"), v(r"C:\b")];
            assert_eq!(one_distro(&mixed), None);
            let two = [v(r"\\wsl.localhost\U\a"), v(r"\\wsl.localhost\D\a")];
            assert_eq!(one_distro(&two), None);
        }
        assert_eq!(one_distro(&[]), None);
        assert!(delete_text(3, "Ubuntu").contains("3 items in Ubuntu (WSL) permanently"));
    }

    /// Del in a WSL folder says the delete is permanent (no Recycle Bin) and deletes for
    /// good when confirmed; nothing is listed or deleted here (the folder is never read).
    #[cfg(windows)]
    #[test]
    fn delete_in_a_wsl_folder_asks_for_a_permanent_delete() {
        use crate::keys::Action;
        use keel_vfs::Kind;
        let dir = VPath::local(std::env::temp_dir());
        let router = std::sync::Arc::new(keel_vfs::Router::new());
        let mut state = crate::state::AppState::new(egui::Context::default(), router, dir);
        let wsl = VPath::local(r"\\wsl.localhost\NoSuchDistro\home");
        state.tab_mut(0).dir = wsl.clone();
        state.tab_mut(0).set_listing(crate::tab::Listing::new(vec![
            crate::tab::test_entry(&wsl, "a.txt", Kind::File, 1),
            crate::tab::test_entry(&wsl, "b", Kind::Dir, 0),
        ]));
        state.run(0, Action::SelectAll);
        state.run(0, Action::Delete);
        match &state.dialog {
            Some(crate::dialogs::Dialog::Confirm { text, on_yes }) => {
                assert_eq!(
                    text,
                    "Delete 2 items in NoSuchDistro (WSL) permanently? WSL folders have no Recycle Bin; this cannot be undone."
                );
                assert!(matches!(on_yes, Action::DeleteRemote(p) if p.len() == 2));
            }
            _ => panic!("no confirm dialog"),
        }
    }

    /// A machine with WSL: `KEEL_WSL_TEST=<distro>` (one that is installed). Opens it
    /// through the sidebar's path and lists it. Without the variable it does nothing.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs WSL: KEEL_WSL_TEST=<distro>"]
    fn opens_a_real_distribution() {
        let Ok(distro) = std::env::var("KEEL_WSL_TEST") else {
            eprintln!("KEEL_WSL_TEST is not set: skipped");
            return;
        };
        assert!(
            keel_term::wsl_distros().contains(&distro),
            "{distro} is in the registry list"
        );
        let root = open(&distro).expect("opens");
        assert_eq!(distro_of(&root).as_deref(), Some(distro.as_str()));
        let listed = keel_vfs::LocalProvider.list(&root).expect("lists");
        assert!(listed.iter().any(|e| e.name == "etc"), "a Linux root");
    }
}
