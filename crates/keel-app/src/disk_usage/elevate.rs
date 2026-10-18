//! **Relaunch elevated**: when a scan could not list some folders because access was
//! denied, the window offers to start Keel again as administrator (Windows, through the UAC
//! prompt), in a window of its own that scans the same folders and drives
//! (`keel --new-window --du-scan <folder> ...`). Never automatic: only the button starts
//! it, and this Keel keeps running.

use std::path::PathBuf;

/// The option that makes a new Keel scan a folder in its Disk Usage window.
pub const SCAN_ARG: &str = "--du-scan";

/// Whether this system has a way to relaunch elevated, and this Keel is not elevated
/// already.
pub fn available() -> bool {
    #[cfg(windows)]
    {
        !keel_search::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The arguments of the elevated Keel for `roots`.
pub fn args(roots: &[PathBuf]) -> Vec<String> {
    let mut out = vec!["--new-window".to_owned()];
    for r in roots {
        out.push(SCAN_ARG.to_owned());
        out.push(r.to_string_lossy().into_owned());
    }
    out
}

/// `args` as one Windows command line: each argument quoted as the C runtime reads it
/// back (backslashes doubled only before a quote, quotes escaped).
pub fn command_line(args: &[String]) -> String {
    let mut line = String::new();
    for a in args {
        if !line.is_empty() {
            line.push(' ');
        }
        if !a.is_empty() && !a.contains([' ', '\t', '\n', '"']) {
            line.push_str(a);
            continue;
        }
        line.push('"');
        let mut slashes = 0;
        for c in a.chars() {
            match c {
                '\\' => slashes += 1,
                '"' => {
                    line.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                    line.push('"');
                    slashes = 0;
                }
                c => {
                    line.extend(std::iter::repeat_n('\\', slashes));
                    line.push(c);
                    slashes = 0;
                }
            }
        }
        // Before the closing quote, every backslash is doubled.
        line.extend(std::iter::repeat_n('\\', slashes * 2));
        line.push('"');
    }
    line
}

/// Starts the elevated Keel scanning `roots` (the UAC prompt asks first).
pub fn relaunch(roots: &[PathBuf]) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe()?;
        keel_search::launch_elevated(&exe, &command_line(&args(roots)))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = roots;
        anyhow::bail!("relaunching elevated is only offered on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_quotes_as_the_runtime_reads_it() {
        let args = args(&[PathBuf::from(r"C:\Program Files\"), PathBuf::from(r"D:\")]);
        assert_eq!(
            command_line(&args),
            r#"--new-window --du-scan "C:\Program Files\\" --du-scan D:\"#
        );
        assert_eq!(
            command_line(&[r#"a "b" c"#.into(), String::new()]),
            r#""a \"b\" c" """#
        );
        assert_eq!(command_line(&[r#"x\"y"#.into()]), r#""x\\\"y""#);
    }

    /// What `--du-scan` is parsed into is what the elevated Keel scans.
    #[test]
    fn the_arguments_parse_back_into_the_roots() {
        let roots = [PathBuf::from("/data/a b"), PathBuf::from("/more")];
        let mut argv = vec!["keel".to_owned()];
        argv.extend(args(&roots));
        let cli = <crate::cli::Cli as clap::Parser>::try_parse_from(argv).unwrap();
        assert!(cli.new_window);
        assert_eq!(cli.du_scan, roots);
    }
}
