# Remotes over SSH

Reference for SFTP remotes, `~/.ssh/config` and jump hosts. Cloud accounts are described in the [README](../README.md#cloud-accounts-bring-your-own-client-id).

## Remotes over SSH

Any host that runs an SSH server with SFTP enabled can be added under **Settings → Remotes** and then appears in the sidebar with a status dot (grey: disconnected, yellow: connecting, green: connected, red: failed). You can copy and move between local folders and remotes, and between two remotes (the data passes through your PC), preview files, and search names in a remote folder with Ctrl+F. Deleting on a remote asks first and is permanent: there is no trash.

### Add your Mac or NAS

1. On the Mac or NAS turn SSH on (macOS: System Settings → General → Sharing → Remote Login; NAS: its SSH or SFTP service). Check that `ssh user@host` works from a terminal.
2. In Keel open **Settings** (`Ctrl+,`) → **Remotes** → **Add**. Enter a label, the host name or IP address, the port (22 by default) and your user name.
3. Pick the authentication: **SSH agent** or **Key file** (for example one from `~/.ssh`, with its passphrase if it has one) are preferred; **Password** also works. Passwords and passphrases are stored in the OS keychain and never in `config.toml`.
4. Optionally set the initial folder and bookmarks. On a Mac, iCloud Drive is at `~/Library/Mobile Documents/com~apple~CloudDocs`.
5. Click the host in the sidebar. The first time, Keel shows the server's host-key fingerprint. Compare it with the one printed by `ssh-keyscan -t ed25519 host | ssh-keygen -lf -` (run it from a machine you trust) and choose Trust only if they match. Keel then records the key in `~/.ssh/known_hosts`.
6. Browse. Bookmarks are listed under the host; copy and paste or drag files between a remote pane and a local pane.

If a known host presents a different key, Keel refuses to connect and says so. Remove the old line from `~/.ssh/known_hosts` only after you know why the key changed.

### OpenSSH configuration and jump hosts

**Use ~/.ssh/config** is on by default for each remote. The Host field can then be an alias from your `~/.ssh/config`, and the editor shows what it resolves to under the field ("connects to host:port as user via jump", or "no ~/.ssh/config entry"). An empty User or key-file path and the default port 22 take the config's values; a user name, another port or a key-file path set in Keel win. With no user anywhere, your local account name is used, as `ssh` does. Turn the switch off to use only Keel's own settings.

What is read: `Host` blocks (`*` and `?` wildcards, several patterns per line, `!` exclusions), `keyword value` and `keyword=value` lines, quoted values, comments, and `Include` (relative to `~/.ssh`, wildcards in the file name, eight levels at most, missing files ignored). A byte-order mark at the start of a file (Windows PowerShell and older Notepad write one) is skipped. As in OpenSSH, a keyword OpenSSH does not know, such as a misspelt `Host`, is an error naming the alias and the file and line, in every block, unless an `IgnoreUnknown` line lists it; files over 1 MiB, and anything that is not a regular file, are refused. As in OpenSSH the first value found for a keyword wins and `IdentityFile` lines add up. Honoured: `HostName` (`%h`), `User`, `Port`, `IdentityFile` (`~`, `%d`, `%u`, `%h`, `%r`; tried in order when the key-file path is empty, before the default keys), `IdentitiesOnly` (with the SSH agent, only the agent keys whose `IdentityFile` has a matching `.pub` file are offered), `ProxyJump` and `ServerAliveInterval` (the keepalive; `0` turns it off).

`ProxyJump` takes a comma-separated route of `[user@]host[:port]` hops, each resolved through the same config (`none` turns it off). Keel signs in to the first hop, opens a forwarding channel through it to the next, and runs the next SSH session inside that channel, up to the remote itself; several hops chain. Each hop has its own host-key check and first-connection prompt, which names the hop, and its key is recorded in `~/.ssh/known_hosts` under the hop's resolved name and port. Jump hosts sign in like the remote (the SSH agent or the key file, with the hop's own `IdentityFile` entries when the key-file path is empty), but a remote's password is never sent to a jump host: a remote that uses a password reaches its jump hosts with the SSH agent's keys first, then the key files.

Not read: `Match` blocks (skipped; the editor shows a note with the file and line), `ProxyCommand` (refused with the alias and the file and line: Keel never runs programs named in the SSH configuration; use `ProxyJump`), `UserKnownHostsFile` (Keel keeps using `~/.ssh/known_hosts`), the system-wide configuration and every other OpenSSH option (they are accepted and ignored). The config is read again on each connection; disconnect and reconnect a connected remote to pick up a change.

### Security notes

Passwords and key passphrases live in the operating system's keychain (Windows Credential Manager, macOS Keychain, Secret Service on Linux) and are never written to `config.toml`, logs or toasts. Host keys are checked against your `~/.ssh/known_hosts`; an unknown key needs your explicit approval and a changed key is a hard error. Uploads are written to a temporary file next to the target and renamed only when complete, so a dropped connection does not leave a half-written file under the real name.
