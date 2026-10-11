# WSL

Keel works with the Windows Subsystem for Linux in two directions: the files of each WSL distribution are in the Windows window's sidebar, and `keel <folder>` typed inside a distribution opens that folder in the Windows window.

Not yet checked on a machine with WSL: everything below is covered by tests that run without WSL (the registry values, the path translation, the window flow with a stand-in for the distribution), and the live steps are under [Checking](#checking).

## Distributions in the sidebar

On Windows with WSL installed, the sidebar has a **WSL** section under **Drives** with one entry per installed distribution, the default one first. Without WSL the section is not shown. Keel reads the list from the registry (`HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`, each distribution's `DistributionName` and the `DefaultDistribution`), never from `wsl.exe --list`, which is slow and starts WSL. The list is read again with the drives (every 30 seconds), so a distribution installed while Keel runs appears. The terminal's shell picker uses the same list.

A click opens the distribution's root folder, `\\wsl.localhost\<name>`, in the active pane (or `\\wsl$\<name>` on Windows versions that only have that name). A stopped distribution is started by Windows on this first access, which takes a few seconds: the entry shows **starting…** meanwhile and the window stays usable. If it has not answered after 30 seconds, or Windows refuses, a toast says "Could not open <name>: <reason>". One distribution opens at a time.

From there it is a folder like a network share (`\\server\share`): the local provider lists, previews, copies, moves and renames as anywhere else. What differs:

- **No Recycle Bin.** A delete in a WSL folder is permanent, and the prompt says so: "Delete 3 items in Ubuntu (WSL) permanently? WSL folders have no Recycle Bin; this cannot be undone." It cannot be undone with Ctrl+Z. A selection that mixes WSL items with others is offered the Recycle Bin, which refuses the WSL items ("This location has no Recycle Bin; nothing deleted"), as on every other network share.
- **The folder may not update by itself.** Windows does not always report changes made inside a distribution to the folder watcher; press F5 to refresh.
- **Search does not cover it.** Everything and Keel's NTFS index do not index WSL folders, so the search tab (Ctrl+F) does not find files there. Filter by typing in a folder, or search from the Linux side.
- **Linux permissions.** File ownership and mode bits are Linux's; **Properties** shows what Windows reports (size, dates, read-only), not the owner or the mode.
- **Folder sizes** are not computed on their own there, as on any network share; **Calculate sizes** works on request.

## `keel` inside WSL

Two ways, both with the Windows Keel installed.

**The Linux build in the distribution** (the [install script](getting-started.md#linux) or the tarball). When it runs inside WSL (`WSL_DISTRO_NAME` is set) and is given a folder or file, it does not open a Linux window: it translates the path and starts the Windows `keel.exe` with it, which hands it to the running window or opens a new one. When a running window took the folder, `keel` exits with `keel.exe`'s exit code (0, or 1 when something failed); when `keel.exe` is still running after 10 seconds it is the new window, and `keel` returns 0 and leaves it open, so the shell gets its prompt back.

| Inside the distribution | Opened in Windows |
| --- | --- |
| `keel /mnt/c/Users/me/Documents` | `C:\Users\me\Documents` |
| `keel ~/src` (any path outside `/mnt/<drive>`) | `\\wsl.localhost\<distribution>\home\me\src` |
| `keel .`, `keel ../docs` | the current folder, resolved first (`..` by name, as `cd` does) |

`--new-window`, `--profile` and `--search` are passed on. `keel.exe` is looked up on `PATH` (WSL appends the Windows `PATH` unless `appendWindowsPath` is off in `/etc/wsl.conf`); set `KEEL_EXE` to its Linux path (for example `/mnt/c/Users/me/AppData/Local/Programs/Keel/keel.exe`) when it is elsewhere. Without either, `keel` says so and opens the Linux window. To open the Linux window on purpose (WSLg), use `keel --here <folder>` or set `KEEL_WSL_NATIVE=1`. Subcommands (`keel search`, `keel du`, `keel mcp`, ...) and `keel` without a folder always run in the Linux process, against the Linux build's own settings and library.

**Or only the Windows Keel**, with an alias in `~/.bashrc` (or your shell's file):

```sh
alias keel='keel.exe'
```

`keel .` then starts `keel.exe` with the distribution's folder as its working folder (`\\wsl.localhost\<distribution>\...`), and Keel opens that folder. Absolute Linux paths (`keel.exe /home/me`) do not work this way; use `keel.exe "$(wslpath -w /home/me)"`, or the Linux build.

Paths that start with `/mnt/<letter>` are taken as drives, as WSL mounts them by default; a distribution with another `automount` root in `/etc/wsl.conf` gets `\\wsl.localhost\...` paths for its drives, which Windows does not serve. Opening a `\\wsl.localhost\...` path needs Windows 11 or Windows 10 21H2 or later.

## Checking

The steps a person with WSL runs to check all of the above (Keel's own tests need no WSL). Use a throwaway profile, so your settings and library are not touched: in PowerShell, `$env:KEEL_CONFIG_DIR = "$env:TEMP\keel-wsl\config"; $env:KEEL_DATA_DIR = "$env:TEMP\keel-wsl\data"`, and start `keel.exe` from that window. For steps 6 and 7 also set `$env:WSLENV = "KEEL_CONFIG_DIR/p:KEEL_DATA_DIR/p"` and open the distribution from that window (`wsl`): Windows programs started inside WSL see only the variables `WSLENV` names, so without it the `keel.exe` those steps start opens your real profile.

1. **The sidebar.** With at least one distribution installed (`wsl --install -d Ubuntu`), start Keel. The sidebar shows **WSL** with each distribution, the one `wsl --list` marks as default first. Run `wsl --shutdown`, then click the entry: **starting…** shows for a few seconds and the pane opens `\\wsl.localhost\Ubuntu` (`bin`, `etc`, `home`, ...). The terminal's shell picker lists the same distributions.
2. **The ignored test.** From a checkout: `$env:KEEL_WSL_TEST = "Ubuntu"; cargo test -p keel-app opens_a_real_distribution -- --ignored --test-threads=1`. It opens the distribution as the sidebar does, lists `etc`, and checks that `keel.exe .` started in that folder asks for it.
3. **Files.** In `\\wsl.localhost\Ubuntu\home\<you>`, create a folder, copy a file in from `C:`, rename it, preview it. Select it and press Del: the prompt says the delete is permanent; confirm, and check it is gone in the distribution (`ls ~`) and that Ctrl+Z says it cannot be undone.
4. **Changes from Linux.** In the distribution run `touch ~/from-linux.txt`. Note whether the Keel pane shows it without F5 (Known limitations say it may not).
5. **A failure.** Run `wsl --shutdown`, stop the WSL service in an administrator PowerShell (`Stop-Service WSLService`, or `LxssManager` for the WSL built into older Windows 10) and click the entry: within 30 seconds a toast says "Could not open Ubuntu: ..." and the pane stays where it was. Start the service again (`Start-Service`).
6. **`keel` inside WSL.** Install the Linux build in the distribution (`curl -fsSL https://raw.githubusercontent.com/Runitupshawty/keel/main/scripts/install.sh | bash`; it installs the latest release, so before 0.20.0 is out build it from a checkout of this version in the distribution instead: `cargo build --release -p keel-app` and run `target/release/keel`), with the Windows Keel running. Run `keel /mnt/c/Windows`, `keel ~`, `cd /tmp && keel .`, `keel "/mnt/c/Program Files"`: each opens a tab in the Windows window on `C:\Windows`, `\\wsl.localhost\Ubuntu\home\<you>`, `\\wsl.localhost\Ubuntu\tmp` and `C:\Program Files` within a few seconds, and `echo $?` prints 0. Close the Windows Keel and run `keel ~`: a new Windows window opens, the prompt comes back after at most 10 seconds, and the window stays open after `exit` from the distribution's shell. `keel --here ~` opens a Linux window through WSLg instead; `KEEL_WSL_NATIVE=1 keel ~` too. `keel du ~` prints sizes in the terminal.
7. **`keel.exe` from WSL.** In the distribution, `cd ~ && keel.exe .` opens `\\wsl.localhost\Ubuntu\home\<you>` in the running window (the single-instance hand-off takes the UNC path), and with no Keel running it starts one there.
8. **A link to a folder.** In the distribution run `mkdir -p ~/kept/inner && touch ~/kept/inner/file && mkdir ~/linked && ln -s ~/kept ~/linked/to-kept`. In Keel delete `\\wsl.localhost\Ubuntu\home\<you>\linked` (permanently, as the prompt says), then check in the distribution that `~/linked` is gone and `~/kept/inner/file` is still there: the link is removed, never followed.
9. **Clean up.** Close Keel and delete `%TEMP%\keel-wsl`.
