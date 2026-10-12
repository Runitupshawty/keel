# The Keel index service (Windows)

Keel's own file index reads each NTFS drive's file table (the MFT) and change journal. Both need administrator rights. Without them, Keel indexes only your folders, or asks for elevation every time it rebuilds (**Index all drives (administrator)** in Settings → General, a one-shot helper `keel --index-service`). The **Keel index service** removes the prompt: installed once, it does the elevated reads for you, and your normal, unelevated Keel builds and searches the full index from what it hands back. It is the same idea as the Everything Service.

This page says what the service does, how to install and remove it, and why it is safe to run. User-facing details of the index itself are in [Search without Everything](files.md#search-without-everything).

## Install and remove

- Settings → General → **Index service** shows its state (Not installed, Installed, Running) with **Install…** or **Uninstall…**. Each asks Windows for administrator rights once.
- From a terminal: `keel --install-service` and `keel --uninstall-service` (they ask for elevation too, and print the result).

Installing copies the running `keel.exe` to `C:\Program Files\Keel\IndexService\keel-index-service.exe` and registers a service named `KeelIndex` ("Keel Index Service") that runs as LocalSystem, starts on demand, and serves only the account that installed it. Keel starts it when it needs the index and the service stops itself ten minutes after the last Keel closes. Uninstalling stops it, deletes it and removes the copy (and the folders, when empty). After you update Keel, install the service again so it runs the new copy; a service that speaks another version of the protocol is refused, and Keel falls back to what it does without it.

If the service is not installed, or cannot be reached, nothing changes: the one-shot **Index all drives (administrator)** still works, and without either Keel indexes your folders.

## What the service does

Only the reads that need administrator rights, for fixed local NTFS drives, and nothing else:

| Request | What it returns |
| --- | --- |
| Hello | the service's process id and Keel version |
| Volumes | the fixed NTFS drives it reads: letter and volume serial number |
| Query journal | the change journal's id and range (`FSCTL_QUERY_USN_JOURNAL`) |
| Read journal | one raw buffer of `FSCTL_READ_USN_JOURNAL` output from a USN, up to 1 MiB |
| Stat | size, size on disk, dates, attributes and link count of up to 1024 files by file id (`OpenFileById`) |
| Read MFT | the volume's file table as the one-shot helper sends it (below): raw MFT records in order, one stream per request, ending with an end or failure frame |

It never returns file contents and never writes. The service **parses nothing a disk holds**: it only locates the MFT (record 0, and the extension records of a fragmented `$MFT`), applies each record's update sequence, blanks everything the index never reads (the contents of small files and alternate streams kept inside the record, extended attributes, security descriptors, the unused tail) and sends the records on, as the helper does (`crates/keel-search/src/ntfs/wire.rs`). Keel decodes the frames, parses every record with its fuzzed parser and builds the index as you, so a hostile volume cannot reach parsing code running as SYSTEM. The journal buffers are passed through unparsed as well: they hold names, ids, reasons, attributes and times only, and Keel parses them. If the file table cannot be read directly the stream says so and carries raw `FSCTL_ENUM_USN_DATA` buffers instead (names and attributes only).

## Protocol

The service listens on the named pipe `\\.\pipe\KeelIndexService`. Each message is a frame: a 32-bit little-endian length, then the payload (requests at most 16 KiB, responses at most 8 MiB). A payload starts with the protocol version and a kind byte; fields follow in a fixed order, little-endian, and must use the payload up exactly. The code is `crates/keel-search/src/ntfs/proto.rs`; its decoder is fuzzed (`index_proto`, [fuzzing.md](fuzzing.md)) and checked to never panic and to encode every message it accepts back to the same bytes.

Both ends decode strictly. Unknown versions or kinds, a count larger than the bytes left, trailing bytes, a drive letter outside `A`-`Z`, a string that is not UTF-8, a file name that is empty, longer than 255 UTF-16 units, `.` or `..`, or holds `\`, `/` or a character below U+0020: each is an error. Nothing is allocated for a count or length before the bytes behind it have arrived.
A `ReadMft` request is answered not with one message but with the helper's frames (`wire.rs`; same length prefix, version byte and kind byte, kinds below 0x80): the volume header (letter and serial), batches of at most 4 MiB of raw records, then an end frame (`full`: from the file table itself, or from the enumeration fallback after a restart frame) or a failure frame. The only other answer is one error response, when the service cannot start the read. Keel feeds the frames to the same receiver as the one-shot helper's: only the volume it asked for (same letter and serial) is taken, record numbers must increase, every record goes through the parser, names must be single path components, and a frame out of order, an unknown kind or the helper's final frame (`Done`, which a service never sends) is a protocol error.

## Security review

The service runs as LocalSystem and answers a less privileged process, so its boundary was reviewed with these questions.

**Who can talk to it.** The pipe's security descriptor is `D:P(A;;GA;;;SY)(A;;0x12019b;;;<your SID>)S:(ML;;NW;;;ME)`:

- a protected DACL (no inherited entries) with exactly two entries: SYSTEM, and the one account the service was installed for. No Everyone, Users, Authenticated Users, Administrators or anonymous access; other users on the PC, including other administrators, cannot open it;
- that account gets `0x12019b`, read and write without `FILE_APPEND_DATA`, which on a pipe is the right to create another instance of it: it cannot stand up a second server under the service's name;
- a medium integrity label with no-write-up: a low-integrity process of the same user (a browser sandbox) cannot write to it;
- `PIPE_REJECT_REMOTE_CLIENTS`: nobody connects over the network.

The account is fixed when the service is installed: Keel passes the SID of the user who clicked Install to the elevated installer, which accepts only a user account SID (`S-1-5-21-…` for local and domain accounts, `S-1-12-1-…` for Microsoft Entra ID, digits and dashes only, checked with `LookupAccountSid` to name a user, not a group). The SID is stored in the service's command line, which only administrators can change, and the pipe's DACL is built from it when the service starts. A SID that tried to inject SDDL (`…)(A;;GA;;;WD`) fails the syntax check before it reaches any descriptor or command line. Installing needs an administrator, so the account was approved by one: in the normal case it is your own account approving its own UAC prompt; when a standard user's Install is approved with an administrator's password, that administrator grants that user the service, which is what the prompt says.

**Who it talks to.** The client opens the pipe with `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`, so the server can identify but never impersonate it. Because the service starts on demand, another process could create a pipe of the same name first. Two checks stop that: the service creates its first instance with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so it fails to start (and Keel does not use it) rather than share a name someone else holds; and Keel asks the Service Control Manager for the process id of the running `KeelIndex` service and refuses a pipe served by any other process (`GetNamedPipeServerProcessId`), then checks that the Hello answer names the same process.

**What a request can do.** Every request names at most one drive letter, and the service answers only for letters that are fixed local NTFS volumes in its own view of drive letters, checked on every request. Drive letters made with `subst`, mapped network drives and `DefineDosDevice` mappings are per logon session; the service runs in session 0 and resolves letters in the global namespace, which only administrators can change, so a user cannot point a letter at something else. Keel in turn uses the service only for a letter whose volume serial matches what Keel itself sees. Requests carry no paths, so there is nothing to traverse; file ids go to `OpenFileById` on the volume handle and only metadata comes back. Request frames are capped at 16 KiB and Stat at 1024 ids; a request that does not decode gets an error and the connection is closed. At most four clients are served at once.

**What it hands back, and how Keel checks it.** The answers give the installing user the names, sizes and dates of every file on the drive, including files in folders their account cannot open (other users' profiles, system folders). That user was an administrator, or approved by one, at install time, so this is information they could already read; it is not file contents. Keel does not show those entries anyway (next section). Keel re-validates every answer: the decoder's checks above, then for each kind: an answer of the kind that was asked for; one stat per requested id; folders without sizes; an MFT stream that is well formed from its volume header to its end frame, for the volume asked for, with records parsed and checked by the receiver (the checks above); journal reads whose next USN never goes backwards and always moves while records come, at most 4096 rounds per poll, with each record's name re-checked. A protocol error drops the connection, and Keel reconnects at most once a minute.

**How much it can do if it is compromised.** The service is registered with only the privileges it uses (`SeChangeNotifyPrivilege`, `SeBackupPrivilege`, `SeManageVolumePrivilege`); the Service Control Manager removes LocalSystem's others (such as `SeDebugPrivilege`, `SeTcbPrivilege`, `SeImpersonatePrivilege`) from its token. It runs the copy in `Program Files`, which only administrators can write, so neither the user nor anything running as the user can replace the executable or plant a DLL next to it; the service's command line is quoted, so `C:\Program.exe` is never tried. Its own DACL is Windows' default for services plus start, stop and query for the installing user (so Keel can start it without a prompt); only SYSTEM and administrators can change or delete it. The service code that runs as LocalSystem is small: the pipe loop, the request decoder, and the volume reads that already existed for the one-shot helper (locating the MFT and blanking records, no parser of the volume's contents).

**What is not covered.** The service does not protect against an administrator, who can replace or reconfigure it at will; nor against the installing user reading metadata of the whole drive, which is what it is for. It is not used by an elevated Keel, which reads the drives itself.

## Folders you cannot open

An index read from the MFT holds every name on the drive. Keel leaves out of search results any file or folder inside a folder your account cannot list, the way Explorer would not show it: before results are shown, each result's folders, from the drive root down, are listed (`FindFirstFileExW`, which never uses a backup privilege) as you; a result under a folder that answers "access denied" is dropped (a folder that is gone does not hide anything). Each folder's answer is remembered for five minutes, so a query checks only folders it has not seen recently. This is on by default; Settings → General → **Index privacy** → "Hide files in folders you cannot open" turns it off. It applies to every search of Keel's own index, with or without the service. When results are hidden, Keel fetches up to twice as many matches to fill the list, so a search for something that matches mostly hidden files can return fewer results than asked for.

## Testing

The service is never installed or started by the test suite. The tests run the protocol over an in-memory connection and over a real named pipe in the test process (created with the service's own DACL for the test user), check that a pipe granted to another account refuses the test user and that a second server cannot take the name, and run install and uninstall against a mock Service Control Manager in a temporary folder. The Service Control Manager calls themselves are compiled and checked by clippy on every build but run only when you install the service.
