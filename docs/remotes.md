# Remotes over SSH

Reference for SFTP remotes, `~/.ssh/config` and jump hosts, and for [FTP and FTPS](#ftp-and-ftps) remotes. Cloud accounts are described in the [README](../README.md#cloud-accounts-bring-your-own-client-id).

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

A name in a server's listing is never trusted as a path. Copying from a remote (SFTP, FTP or a cloud account) into a local folder refuses, before anything is written, any entry whose name is not one plain file name there: on Windows a name with `\`, `:` or `<>"|?*`, a control character, a trailing dot or space, or a device name such as `CON` or `aux.txt`; on Linux and macOS a name with a control character. The error names the entry ("refusing an unsafe name in …"); rename it on the server to copy it. Every target is also checked to lie inside the destination folder before it is written.

## FTP and FTPS

NAS boxes, web hosts, cameras and old file servers that speak only FTP can be added as remotes too. They appear in the sidebar like SFTP hosts (same status dot, bookmarks, Connect, Disconnect, Edit and Copy address), and you browse, preview, search names in, copy to and from, rename and delete on them the same way.

### Add an FTP or FTPS server

1. **Settings** (`Ctrl+,`) → **Remotes** → **Add host…**, then pick the **Protocol**: **FTPS** (FTP over TLS) or **FTP**. The port follows the protocol (21) until you change it.
2. Enter a label, the host name or IP address, and either a user name and password or **Anonymously** (Keel signs in as `anonymous` with an email-shaped password, as public servers expect). The password goes to the same credential store as SFTP passwords (the OS keychain), never to `config.toml`.
3. Optionally set the initial folder and bookmarks, then Save. Paths on the host are `ftp://<host id>/<path>`, for FTP and FTPS alike.

**FTPS** asks the server for TLS on the control connection (`AUTH TLS`, explicit FTPS) and protects every data connection (`PROT P`); on port 990 it speaks TLS from the first byte (implicit FTPS). A server that refuses TLS is an error: Keel never falls back from FTPS to plain FTP, and never continues a failed TLS handshake.

**Plain FTP sends the password and the files unencrypted**: anyone on the network path can read both. The Add remote dialog says so next to the protocol choice ("FTP sends the password and the files unencrypted. Prefer FTPS or SFTP."), and the remote's sidebar tooltip and its line in Settings → Remotes repeat it. Use plain FTP only on a network you trust, for servers that offer nothing else.

### Certificates

An FTPS server's certificate is checked against the operating system's trust store (the same checks as the cloud accounts' HTTPS). A certificate that fails (self-signed, expired, issued for another name: common on NAS boxes) gets the same kind of prompt as an unknown SSH host key, with the reason and the certificate's SHA-256 fingerprint. Compare the fingerprint with the one the server shows (for example its admin page, or `openssl s_client -connect host:21 -starttls ftp | openssl x509 -noout -fingerprint -sha256` run from a machine you trust) and choose **Trust and connect** only if they match. Keel then remembers that fingerprint for that host and port in `ftps_known_hosts` in its configuration folder (`KEEL_CONFIG_DIR` when set), and connects to it without asking again. If the server later presents a different certificate that again fails verification, Keel refuses to connect and says so; delete the host's line from `ftps_known_hosts` only after you know why it changed. With no Keel window open to ask (keel-daemon, the CLI), an unverified certificate is refused.

### How transfers work

- **Passive mode only.** The data connection always goes to the address of the control connection (a server behind NAT that names an inside address in its `PASV` reply works, and a server cannot send Keel's data connection to a third machine); IPv6 servers use `EPSV`. Active mode (`PORT`) is not supported.
- **Listings** use `MLSD` when the server offers it, else `LIST` in the UNIX (`ls -l`) or DOS (IIS) format, with `-a` on UNIX servers so dot files show. Lines over 8 KiB and folders over a million entries are refused. Dates from `LIST` are shown in UTC to the minute (a server's own time zone is unknown), and `LIST` dates without a year are placed in the last twelve months. A symbolic link shows as the folder or file it points to (checked with `CWD`, for the first 256 links of a folder).
- **Downloads** can start in the middle of a file (`REST`), so previews of large files and resumed copies do not read the start again.
- **Uploads** go to a staging name beside the target and are renamed into place when complete (and size-checked where the server answers `SIZE`), like SFTP uploads: a dropped connection never leaves a half-written file under the real name. A long copy that stops (Cancel, a lost connection, a closed window) continues where it stopped with `APPE` or `REST` when it runs again and the server reports the staging file's size, else it starts that file over.
- **Deletes are permanent**: FTP has no trash, and the delete dialog says so ("This cannot be undone").
- Connect, reply and data timeouts are 20 seconds, as for SFTP; a transfer holds its own connection, so browsing during a copy opens another one.

### What FTP remotes do not support

Mounting an FTP remote as a drive, using an FTP folder as a library source (and so hashing its files, protection counts and the duplicate finder over it), `~/.ssh/config`, key files and agents, **Open terminal here**, active mode and free-space figures. A library source on an FTP path is refused with "library sources are not supported for FTP remotes", and the library's **Add a library source** lists only SFTP hosts and cloud accounts.
