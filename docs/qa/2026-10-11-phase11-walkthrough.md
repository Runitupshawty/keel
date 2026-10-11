# QA walkthrough: Phase 11 (0.20.0 candidate)

Date: 2026-10-11. Candidate: `rel20` after the review fixes. Machine: the owner's Windows desktop (debug and release builds), plus a throwaway FTP container on a Linux machine of the owner's for the FTP checks. Every run used a throwaway profile (`KEEL_CONFIG_DIR`, `KEEL_DATA_DIR`, `KEEL_NET_SECRET=memory`).

## Automated flows

`cargo test -p keel-app` runs the kittest walkthrough flows (`crates/keel-app/src/walkthrough_tests.rs`), now including the Phase 11 flows: Ctrl+P finds a file, selects and previews it, and Tab filters to folders; the WSL section with a fake distribution list (default first, "starting…", navigation, the error toast, the section hidden without WSL); the Add remote dialog with protocol FTP and its clear-text warning; the restore dialog. Result at the reviewed tree: 400 passed, 28 ignored (the ignored ones need a live server, WSL, or the Developer Mode symlink privilege). The other crates: keel-vfs 167 + 97, keel-core 211, keel-api 66, keel-daemon 35, keel-search 48, keel-term 18.

## Live checks

| Flow | Where | Result |
| --- | --- | --- |
| FTP: list, upload, download, ranged read, delete through the provider | vsftpd 3.0.2 in a container (plain FTP, LIST only) | pass |
| FTPS explicit (`AUTH TLS`, session reuse required) and implicit (port 990) | same container, self-signed certificate | pass; the fingerprint prompt matched the server's certificate; implicit mode needed `PBSZ`/`PROT P` (fixed before the review) |
| FTP remote in the window: add in Settings, open from the sidebar, list a folder | desktop, env-gated flow `live_ftp_remote_in_the_window` | pass (plain FTP; the certificate dialog was not exercised in the window) |
| Listing names that escape the destination (`..\x`, `c:d`, `CON`, trailing dot) | review probe, then the in-process FTP and SFTP tests | refused on Windows after the fix (B1); each lands as one file on Unix |
| Hostile server: endless blank `LIST` lines | review probe | bounded by the 64 MiB listing cap after the fix |
| Parallel copy: 3,000 one-byte files, plain and through a plan | desktop, release build | plain 0.84–1.0 s (about 3,000 files/s, was 3.7 s); plan 2.7–3.1 s |
| Parallel copy: cancel part way; a locked file in the middle | desktop | no `.partial` left; the job stops before the locked file; Landed lists exactly what landed |
| Ctrl+P over 20,000 generated files | desktop, env-gated `KEEL_LIVE_JUMP_DIR` | first batch after 245 ms, full walk 853 ms, slowest keystroke 31 ms while walking |
| Backup and restore of a 300-file library with 11 tagged files | desktop, CLI and through the daemon | refused under the existing name; `--as copy` restored with the tags; after deleting the library folder the restore brought every tag back |
| `keel` inside WSL (path translation, `KEEL_EXE`, exit codes, the 10 s hand-off) | Linux machine with `WSL_DISTRO_NAME` set and a fake `keel.exe` | pass for every case in docs/wsl.md |
| WSL section and `\\wsl.localhost` on a machine with WSL | — | not run: WSL is not installed on the desktop (see docs/wsl.md "Checking") |
| Installer restarts the daemon it stopped | hermetic tests in `scripts/test-install.ps1` and `test-install.sh` | pass |

## Left for the owner

- The WSL live steps in docs/wsl.md once WSL is installed.
- The FTPS certificate dialog in the window against a server with a self-signed certificate.
- A libFuzzer run of the two FTP listing targets on Linux (the in-tree runs did about 80,000 cases each).
