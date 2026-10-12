# Files, archives, search and the window

Reference for the file panes and everything around them. The [getting-started guide](getting-started.md) introduces the window and everyday tasks.

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| Ctrl+Z | Undo the last file operation (rename, move, copy, new item, delete to the trash, a [folder operation](#folder-operations)); see [Undo](getting-started.md#copy-move-and-delete) |
| Ctrl+F2 | Bulk rename the selection (pattern, find/replace, case; Ctrl+Z undoes it) |
| Ctrl+, | Settings |
| Ctrl+Shift+T | Tag picker (library) |
| Ctrl+Shift+A | Tag assign mode: 1–9 toggle tags on the selection, 0 or Esc leaves (library) |
| Ctrl+D | Toggle favorite (library) |
| Ctrl+I | Inspector: what the library knows about the selected item (see [Inspector](#inspector)); in a search tab, Match case (see [The search tab](#the-search-tab)) |
| Ctrl+B, Ctrl+U, Ctrl+M, Ctrl+R | In a search tab: Match whole words, Match path, Match diacritics, Regular expression |
| Ctrl+Shift+O | Open with… (pick an app for the selected files) |
| Shift+F10 | The Windows menu for the selection (Windows; see [Windows shell menu](#windows-shell-menu)) |
| Ctrl+Shift+Z | Show or hide the drop zone |
| Ctrl+Shift+S | Stash the selection in the drop zone |
| Ctrl+Shift+Alt+K | Global hotkey: bring Keel to the front (configurable) |

These are the defaults. The full list, with your bindings, is in the command palette (Ctrl+Shift+P) and in Settings → Keyboard. Cmd replaces Ctrl on macOS.

### Changing the keys

Settings → **Keyboard** lists every command Keel has: the command palette's, plus the keys the palette does not list (the palette itself, Undo, Backspace, Space, Esc, the cursor keys, Leave the terminal). Type in **Search commands or keys** to narrow the list by name or by key ("ctrl+e" finds what Ctrl+E does).

- **Record** adds a key: press it (Esc cancels the recording). A command can have several keys, or none.
- Click a key (`Ctrl+E ×`) to remove it. **Reset** puts one command's default keys back, **Reset all** every command's. Commands you changed are shown in bold.
- A key another command already has is not taken silently: Keel names that command and offers **Use it for "…"** (the key moves) or **Cancel**. Keys only clash where they work: F6 is Switch pane in the file panes and Leave the terminal in the terminal, and that is not a conflict; ``Ctrl+` `` (the terminal) works in both. A search tab's keys (the five search toggles and Load more, "Search: …") work only in a search tab, where they come first: Ctrl+I is Match case there and the Inspector elsewhere, and that is not a conflict either.
- Some commands must stay reachable: the command palette, Esc (closes the filter, a rename, dialogs and menus) and Leave the terminal always keep at least one key, so removing or moving their last key is refused. Plain Esc belongs to Esc alone.
- Some keys cannot be bound: Ctrl+C, Ctrl+X, Ctrl+V, Ctrl+Insert, Shift+Insert and Shift+Delete (the clipboard's; Copy, Cut and Paste always have them and can get more), Ctrl+= / Ctrl+- / Ctrl+0 (the UI scale), Tab and Shift+Tab (keyboard focus), and a letter, digit or punctuation key without Ctrl or Alt (it types into the filter).
- **Export…** writes every command's keys to a TOML file; **Import…** reads one back (commands it leaves out get their defaults; unknown commands and keys are skipped and named). The file is the same `[keymap]` table that config.toml keeps.

Keys are per profile: config.toml holds only the commands whose keys differ from the defaults, as `[keymap]` with the command's id (shown when you hover its name) and a list of keys, for example `"view.theme" = ["Ctrl+K"]` (an empty list: no key). A hand-edited entry that would break the rules above is skipped at start, with a notice, and the original file kept as `config.toml.bad`. "Ctrl" in the file is Cmd on macOS. Keys inside popups (Ctrl+P's Tab and F5, assign mode's number keys, the media viewer's) and the global hotkeys are set elsewhere.

## Columns view and drop zone

Switch a pane to **Columns** from the view buttons in the pane header. Column 0 is the tab's folder; selecting a folder lists it in the next column, and selecting a file shows its preview in the last one. Left/Right move between columns, Enter opens a folder, Up or Backspace step back one column, and the column dividers can be dragged (widths are saved). All file actions work on the column you are in.

The **drop zone** is a strip above the status bar (Ctrl+Shift+Z shows or hides it; it also appears while you drag rows). Drop rows on it, or press Ctrl+Shift+S to stash the selection, then navigate anywhere and use **Paste here** (copy) or **Move here** into the active pane's folder; **Clear** empties it. Items that no longer exist are greyed and skipped, and moved items leave the strip only once they have really moved. The stash is saved with the session. "Reduce motion" in Settings → General turns off the animations.

## Dragging files to other apps

Drag rows out of Keel's window onto another app (a file manager, a mail draft, an editor, a chat window) to hand it the files. The other app copies them; hold its move key while you drop to let it move them instead: Shift on Windows and Linux, Command on macOS. Keel never moves or deletes the files itself. When the drop is over it refreshes the folder, and a move only shows once the other app has really taken the files away (if they are all still there, nothing was moved). Esc cancels the drag. Dropping the files back onto Keel does nothing, and only files on this computer can be dragged out (not files on SFTP hosts, cloud accounts or inside archives).

- **Windows**: a native file drag, as from Explorer.
- **macOS**: a native file drag with each file's icon under the pointer; Finder, Mail, TextEdit and other apps receive the files' URLs.
- **Linux**: works on X11. Keel offers the files as `text/uri-list` over XDND, which GNOME Files, Dolphin, Thunar, browsers, mail apps and most editors take. There is no drag image: the pointer keeps its usual shape while you drag. In a Wayland session dragging out needs Keel to run through XWayland: start it with `WAYLAND_DISPLAY` unset (`env -u WAYLAND_DISPLAY keel`). From a native Wayland window a row drag stays inside Keel (it still drops onto the other pane or a folder), since Wayland only lets the toolkit start a drag. A desktop that takes drops through a proxy window (icons on the desktop) is not a target.

## Inspector

The preview panel has two tabs, **Preview** and **Inspector**. Ctrl+I (or **Show inspector** in the command palette) opens the panel on the Inspector, and hides it when the Inspector is already showing; F3 does the same for the Preview. Like the preview, the Inspector follows the active pane's cursor (or the first selected item). It only reads: nothing in it changes a file.

- **Overview**: name, kind, size, modified date, and the path with a **Copy** button. It comes from the listing, so it shows at once and works for any file, in the library or not.
- **Library**: the source and the volume it is on (with the volume's state, and *backup* when it is marked as a backup drive), the content id with a **Copy** button, when the index first saw the item, when its bytes were last hashed, and its protection: how many copies the library knows in how many failure domains, and whether it is backed up ([Protection](library.md#protection)).
- **Copies**: every other file with the same content, in any source or on a paired device (a device's own claim says so on hover and is not counted). Click a row to open its folder with the copy selected. A file that is not hashed yet says so: its copies are not known.
- **History**: the operations that touched the item, newest first: copied (to or from), moved (also by a merge, flatten or organize), renamed, deleted, tagged and untagged, each with its time and where it went or came from. It follows the item back through its moves and renames. The first 50 show; **Show all** lists up to 1,000.
- **Tags**: its tags as chips, ★ when it is a favorite, and **Tags…** to open the tag picker (Ctrl+Shift+T).
- **Folder** (for a folder instead of Copies): its size, files, folders and its biggest child, from the library's folder totals ([Folder sizes](#folder-sizes-and-the-sizes-view)).

The library is asked 150 ms after the selection stops moving, on a background thread, so moving through a list stays quick. With the window attached to keel-daemon the same answer comes through the daemon (`library.inspect`, [api.md](api.md)). Outside the library's sources (or with the library off) the Overview still works and the other sections say **Not in the library**. *First seen* and *last hashed* are recorded from Keel 0.21 on: items indexed before show *not recorded* until they are seen or hashed again, and the history starts with what the operation log holds (tagging is logged from 0.21 on).

## Windows shell menu

On Windows, right-click files on this computer and pick **More options** at the end of Keel's menu, Shift+right-click them, or press Shift+F10, to get the menu Explorer shows: the handlers your installed apps add (archivers, editors, version control), **Send to**, **Open with**, **Share** and the rest. Shift+right-click and Shift+F10 include the extended commands Explorer shows with Shift held, such as **Copy as path**. Shift+F10 opens the menu at the cursor row.

The same commands are in the command palette (Ctrl+Shift+P) while files on this computer are selected, as **Shell: …** entries named by their place in the menu, for example `Shell: Send to > Documents` or `Shell: Open with > Notepad`. Type `Shell:` to list them: the entries are added the first time you type `shell` in the palette (loading the handlers can take a moment), never when it opens or in the background, and they are dropped when it closes. Commands whose menu item has no text (some apps draw their own) are only in the popup.

Keel takes out the commands it has its own version of, so its rules always apply: Delete (Keel's moves to the Recycle Bin and asks first), Rename, Cut, Copy, Paste, Create shortcut and Properties (Keel's Properties dialog, whose **Windows Properties…** button opens Explorer's sheet). The Windows menu works on files and folders in one folder on this computer, network shares included. A name ending in a dot or space is refused: the shell would read it as another file's name (`report.` as `report`), and Keel also checks that the item the shell resolves is the one selected. For files on an SFTP host, a cloud account or a paired device, inside an archive, in the library view, or search results from several folders, **More options** is greyed and says why: a handler would only act on a temporary copy.

The commands come from shell extensions, small programs other apps install into Explorer, and they run inside Keel as they run inside Explorer. A badly written one that crashes closes Keel's window, and one that hangs freezes it until it returns. That is why Keel builds the menu only when you ask for it. If a command misbehaves, the app that added it is the one to update or remove. macOS and Linux have no such menu, so the entry is not shown there.

## Archives

Press Enter or double-click a `.zip`, `.jar`, `.7z`, `.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`, `.tar.xz`, `.tar.zst` or `.rar` file to open it as a folder (an archive inside an archive opens too). Up leaves the archive; preview works on the files inside. Right-click for **Extract here**, **Extract to folder**, **Extract to…**, **Add to "name.zip"** and **Compress to zip…** (pick Zip, 7z, Tar or Tar.gz in the dialog; the last choice is kept as `default_format` under `[archive]` in `config.toml`). Select an existing zip, 7z, tar or tar.gz together with other items from the same folder, or drop files on the archive row, to get **Add to "name"…** (asks first, runs as a cancellable job). Extraction asks before overwriting and refuses entries that would land outside the target folder. It also works into folders on SFTP hosts and cloud accounts: paste or drag entries there, extract an archive that lives there with Extract here or Extract to folder, or open the target folder in the other pane and pick **Extract to the other pane**. Each entry is streamed from the archive to the host and renamed into place once complete.

Inside a `.zip` or `.jar` on this computer you can work as in a folder: Delete, Rename (F2), Cut, Copy and Paste, drag and drop, New folder and New file, and paste or drop files and folders from anywhere (local, SFTP, cloud, another archive) into it. Each operation rewrites the archive once: the new archive is written beside the old one, synced and renamed over it, so a cancel, an error or a crash leaves the original as it was. Entries that stay are copied byte for byte (stored entries stay stored, password-protected entries are never decrypted, zip64 archives stay zip64); new files are deflated. Deleted entries go for good, and their folder stays. Large archives take as long as copying them once.

Everything else is read-only: 7z, tar and RAR archives, an archive inside an archive and an archive on an SFTP host or a cloud account (copy it to this computer first). You can extract from any of them and add files and folders to a zip, 7z, tar or tar.gz with Add to (entries of the same name are replaced; a 7z is re-encoded, a tar is streamed, and the old archive stays untouched until the new one is complete). RAR is read through libunrar and never written. Moving entries out of an archive is not supported: copy them, then delete. Password-protected entries are marked with a lock and cannot be previewed.

## Recycle Bin and Trash

Open **Recycle Bin** (Windows) or **Trash** (macOS; Linux, any freedesktop-compliant desktop) in the sidebar. Columns: Name, Original location, Size, Deleted on. **Restore** moves the selected items back to their original folders; if something with the same name is already there Keel stops and says so instead of overwriting it. **Delete permanently** and **Empty Recycle Bin** remove items for good and ask first (Empty shows the item count). The folder is read-only otherwise: nothing can be pasted, created or renamed in it, and a trashed folder cannot be browsed until it is restored. The preview panel shows trashed files where the system keeps them at a normal path (Windows `$R` files, Linux `files/`, every item on macOS); otherwise it says there is no preview.

**macOS.** The Trash entry lists your Trash folder (`~/.Trash`) and the Trash folder of each mounted volume (`/Volumes/<name>/.Trashes/<your user id>`) as plain folders, so Size and Deleted on (the time the item was moved there) are shown but Original location stays empty: macOS keeps Finder's "Put Back" location where other apps cannot read it reliably. Keel moves files to the Trash itself (through NSFileManager, not by asking Finder), so it needs no Automation permission and makes no sound, but Finder's **Put Back** does not offer itself for the items Keel trashed: use **Restore to…** or drag them out. **Restore to…** asks for a folder and moves the selected items into it; it stops before moving anything when a name is already there or two items share a name, and an item on another volume than the folder you pick is refused (pick a folder on that volume, or copy it out). **Delete permanently** and **Empty Trash** remove the items for good after the same confirmations. macOS lets an app read the Trash only with Full Disk Access: without it the Trash tab says so (System Settings → Privacy & Security → Full Disk Access).

## Terminal

A terminal pane sits under the file panes. It starts in the active pane's folder and, with "Follow pane" on, changes directory when you navigate. Shells: PowerShell, Windows PowerShell, cmd and each installed WSL distribution on Windows (the same list as the sidebar's WSL section, see [WSL](wsl.md)); `$SHELL`, zsh or bash on macOS; `$SHELL`, bash or sh on Linux.

| Key | Action |
| --- | --- |
| ``Ctrl+` `` (backtick; Cmd on macOS) | Open, focus or hide the terminal. Hiding keeps the shell running; the x button ends it |
| `F6` or `Shift+Esc` | Leave the terminal and return to the file panes |
| `Esc` | Goes to the shell (vim, less, fzf) |
| Context menu, "Open terminal here" | Focus the terminal in the current folder |

"Follow pane" sends a `cd` only when the shell looks idle at a plain prompt; heavily customised prompts (oh-my-posh and similar) can prevent it.

## The search tab

Ctrl+F opens a search tab. Its bar has the backend (System, or Library when the library is on), the query box, Everything's five search toggles and a **Search** menu:

| Toggle | Key | What it does |
| --- | --- | --- |
| Match case (`Aa`) | Ctrl+I | `Report` does not find `report` |
| Match whole words (`ab`) | Ctrl+B | `port` does not find `report.pdf`; `report` still finds `my report.pdf` |
| Match path (`\`) | Ctrl+U | Every term matches the full path (folders and name), not only the name: `old pdf` finds `old\report.pdf` |
| Match diacritics (`é`) | Ctrl+M | Off (the default), letters match their accented forms both ways: `cafe` finds `café` and `café` finds `cafe` |
| Regular expression (`.*`) | Ctrl+R | The whole query is one regular expression over the name (or the path with Match path) |

The keys work in a search tab, also while typing in its query box (elsewhere Ctrl+I is the Inspector); Settings → Keyboard changes them like any other key (see [Changing the keys](#changing-the-keys)). The toggles are per tab and also in the command palette ("Search: Match case", …); a new search tab starts with the ones set under Settings → General → **New searches** (per profile). With Everything as the backend Keel hands the toggles to Everything (Match case, path, whole words and regex as its own search switches, Match diacritics as its `diacritics:` modifier around the query); with Keel's own index and the plain walk Keel applies them itself. `locate` uses only Match case and Regex; Spotlight uses none of them. The library search has its own terms, so the toggles are greyed there.

The query box's hint names what the backend understands: Everything's syntax with Everything, names, `*` and `?` wildcards, `"exact phrase"`, `folder:`, `in:<folder>` and `regex:` with Keel's index (it has no `ext:` or `dm:` yet; those match as plain text), the library's terms with the library.

Results:

- The first 1,000 results show at once ("first 1000 results"). **Load more** (in the bar and the Search menu) asks for four times as many, up to **Most results** in Settings (10,000 by default, at most 1,000,000). The list draws only the rows on screen, so long lists scroll smoothly; Keel's NTFS index looks up the size and date of the extra rows on a worker.
- The parts of each name the query matched are highlighted, and with Match path the parts of the Folder column too. Wildcard terms highlight their fixed parts; `ext:`, `in:` and other `function:` terms are not highlighted.
- The status bar shows the number of results, their total size, and the selection's count and size.
- Ctrl+Enter (also from the query box) or **Open path** opens the result's folder with it selected. **Copy name** copies the file names; **Copy path** copies full paths, in double quotes when Settings → General → **Copy path: In double quotes** is on.

Settings → General → **Search**: **Search as you type** (on; off, Enter runs the query) and its delay (150 ms), and **Match whole filename when using wildcards** (on: `*.pdf` must match the whole name; off: a wildcard term may match anywhere in it, so `*.pd` finds `report.pdf`). With Everything as the backend its own wildcard setting applies.

## Search without Everything

On Windows, when Everything is not running, Keel uses its own index: it reads the NTFS file table and keeps it current from the USN journal, and stores it in the cache folder. Queries are substring, glob (`*.pdf`) or `regex:`, with `folder:` and `in:<path>` filters, and the [search toggles](#the-search-tab).

Reading the whole file table needs administrator rights. Until you grant them Keel indexes your home folder (recursively) plus one level of each drive's root. Settings → General → **Index all drives (administrator)** asks Windows for elevation once, runs a helper (`keel --index-service`) that builds the full index, and hands the files back to your user; later starts need no elevation. The status bar names the active backend and shows its state. If Everything is running it is used instead and the button is greyed out.

On macOS and Linux Keel uses Spotlight (`mdfind`) or `plocate`/`locate` when they answer. Otherwise (and on Windows when nothing else works) it keeps its own **name index** of your home folder: a walk that skips hidden and git-ignored entries, saved in the cache folder (`<cache>/index/walk-<hash>.db`) so the next start is instant, and kept current from file system events in half-second batches. It is rebuilt when it is older than 7 days. The status bar says "indexing N files…" while it builds; searches work as soon as the first build finishes. Queries use the same syntax as above (substring, `*.pdf`, `regex:`, `folder:`, `in:<path>`).

## Finding folders and files (Ctrl+P)

Ctrl+P opens the finder. Type a few letters of a folder's or a file's name (in order, not necessarily next to each other; the whole path counts) and press Enter: a folder opens, and a file opens its folder with the file selected and the preview shown. Ctrl+Enter opens the result in a new tab, Shift+Enter in the other pane (in a new tab when there is one pane). Each result shows its icon, its name and, dimmed, the folder it is in. Tab switches between **Folders**, **Files** and **Both** (the default); the choice is kept per profile.

The names come from three lists, each bounded:

- **Folders**: every folder the search index knows (Everything, Keel's own index, Spotlight or `locate`), else a walk of your home folder (its first 50,000 folders).
- **Files here**: a walk of the active pane's folder and everything below it, skipping hidden and git-ignored files, up to the first 50,000 files (the popup then says "first 50,000 files here"). Only folders on this computer are walked; in an SFTP, cloud, device or archive folder this list is empty.
- **Library files**: the indexed files of your library's online sources, each source's newest first, up to 100,000 (the popup then says "first 100,000 library files"). Offline sources are left out. Attached to keel-daemon, the names come through the API (`files.names`).

Each list is read in the background when the popup opens and kept for 10 minutes; F5 in the popup reads them all again. You can type at once: until a list arrives the popup says what it is still indexing, with the count so far, and results fill in as the files come. On equal matches the shorter path comes first, and files of the current folder come before library files.

## Folder sizes and the Sizes view

Folders show their total size in the Details **Size** column, and sorting by size puts folders in order of their totals (folders whose total is not known yet come last). The status bar adds up the selected folders' totals, when known.

- **In a library source** the totals are kept with the index ([Library](library.md#library)): they are there as soon as the source's first walk has finished, and follow every change the watcher or a remote check applies.
- **Elsewhere** (the library off, or a folder in no source), **Calculate sizes** (the folder's context menu, or the command palette) walks the shown folder's subfolders as a job in the jobs panel, which you can cancel. The totals are kept until Keel closes, by path and the folder's own modified time: adding, removing or renaming something directly in a folder drops its total until it is walked again, but a change deeper down (a file that grew two folders below) does not change the folder's own time, so its total can be out of date. Run **Calculate sizes** again to walk afresh. A walk keeps the totals of every folder below the ones it walked, so going down into them is instant. Calculate sizes starts on its own only in folders on a fixed disk of this computer outside the library, and only with Settings → General → **Folder sizes: In local folders** on (off by default, because walking a big tree costs disk time); never on a network share (a `\\server\share` path or a mapped drive), a removable drive, an SFTP host, a cloud account or a device.

The Size column shows `…` while a folder is being walked and stays empty while its total is unknown.

**The Sizes view** (the **Sizes** button next to **Media**, or "Sizes view" in the palette) lists the folder's children biggest first, folders and files together. Each row has a bar with its share of the folder, its file count, and its share of the space used on the drive (the drive's own numbers; empty on hosts and cloud accounts). Enter goes into a folder and Backspace back up, as in Details. Outside the library, opening the Sizes view on a folder on a fixed disk walks it once; on a network share, a removable drive, an SFTP host or a cloud account it shows a **Calculate sizes** button instead, since walking those is slow or costs requests. There is no treemap.

Sizes are apparent sizes (a file's length, not the space it takes on disk). A file with several hard links counts once for each path, a cloud placeholder by the size the service states, and links (symlinks, junctions) are neither counted nor followed. Inside a library source a total counts what the index holds: ignored files, and hidden files when the source leaves them out, are not in it. A folder that cannot be read counts as empty, and the job says how many there were; `keel du` shows a child folder it cannot read as `?` and goes on. `keel du <path>` prints the same from a terminal ([Daemon, CLI and MCP](daemon.md#daemon-cli-and-mcp), and `library.folder_sizes` in [api.md](api.md)).

### Disk usage scans

`keel du --scan <folders or drives…>` (or `--all-drives`) runs Keel's disk usage scanner, the engine behind the coming Disk Usage window (Phase 17). It needs no library and works the way WinDirStat does:

- **Several roots at once**, folders and whole drives, walked in parallel (one task per folder on a work-stealing pool, so a wide drive keeps every core busy) into one result. `--all-drives` scans every local fixed drive (elsewhere than Windows: every mounted volume that is not removable or a network share).
- **Size on disk** next to the size: on Windows the allocation the folder listing reports (no file is opened), and for compressed and sparse files `GetCompressedFileSizeW` rounded up to the volume's cluster; elsewhere the blocks the file has.
- **Hard links count once**: a file with several names adds its size to the first one in the tree only; the other names are listed with the note "counted elsewhere (hard link)".
- **Links are not followed**: symlinks, junctions and mount points are listed (as `link`, with a note saying which) and add nothing. `--follow-links` follows them; a link leading back to a folder it is in is listed as a link loop and not entered, so a scan always ends.
- **Cloud placeholders are never downloaded**: an online-only file counts by its stated size with nothing on disk, and an online-only folder is not listed (note "cloud placeholder").
- **Free space and unknown space** for each drive scanned from its root: unknown is the drive's used space minus what the scan found on disk (system files Keel cannot read, other users' folders, files changed since).
- `--exclude GLOB` leaves out files and folders by name (`*` and `?`; repeat it for several; without case on Windows). The scanner also filters by path, by a regular expression on the path, by minimum size, by age (older or newer than), and leaves out hidden or system items; the Disk Usage window will offer those.
- `--top N` lists the N biggest items of each root (20 by default), then its free and unknown space and its total, and at the end the file and folder counts and the time taken; while it runs a terminal shows the files and bytes found, files per second and the folder being read.
- `--save FILE` writes the whole scan to a file (gzip-compressed JSON, versioned), and `keel du --load FILE` shows it again later without walking. A scan file written by a newer Keel is refused with a message saying so.
- `--json` prints the same as one JSON document ([api.md](api.md#cli)).

A folder that cannot be read counts as empty and the last line says how many there were; folders deeper than 512 levels are not entered.

## Folder operations

Right-click a folder (or a file, for the last two) and open **Folder operations**, or type the name in the command palette (Ctrl+Shift+P):

| Operation | What it does |
| --- | --- |
| **Merge into…** | Select two or more folders, pick the one they merge into: everything in the others moves into it, and the folders left empty are removed |
| **Flatten…** | Every file in the folder's subfolders, at any depth, moves up into the folder; the subfolders left empty are removed |
| **Organize by…** | The folder's files (not its subfolders) move into subfolders by **Type** (Images, Videos, Audio, Documents, Archives, Other, by extension), **Year** (`2024`) or **Year and month** (`2024-03`) of their modified date; missing subfolders are made |
| **Attributes…** | Hidden and read-only (Windows; hidden also on macOS), and on Linux and macOS the mode bits in octal (`755`). The checkboxes start at the items' current values (filled in grey when the selected items differ); only what you change is set. Not recursive |
| **Create link…** | A link to the selected file or folder, next to it: a **Junction** (Windows, folders) or a **Hard link** (files, on the same drive), or a **Symbolic link** |

With nothing selected, Flatten, Organize and Attributes work on the folder shown. Merge, Flatten and Organize ask what happens when a name is taken: **Skip** (the item stays where it was, and so does its folder), **Keep both** (the new one gets a free name such as `a (2).txt`) or **Overwrite**. Two items of one operation never land on one name (letter case aside, on every system): with Skip or Keep both the later ones get the next free name (`same (2).txt`), and with Overwrite, which would replace the operation's own files, the operation is refused, naming each item that would. Each operation then opens the library's preview, which lists every move with its target, every folder it makes or removes, and every name that is taken; Enter or **Confirm** runs it as a job in the jobs panel, with progress and cancel. A cancelled operation leaves every file in one place, moved or not. If the folder changed after the preview (a file added, removed or renamed), the operation does not run: the fresh preview is shown to confirm instead. Links (symbolic links, junctions) inside the folders are never moved and never followed; a folder that still holds one stays.

Symbolic links on Windows need Developer Mode (Settings → System → For developers) or an elevated program, and Keel never elevates: the dialog says so and offers a junction (folders) or a hard link (files) instead, which need neither.

**Ctrl+Z** undoes the last of these like any file operation: merge, flatten and organize move every item back (refusing, without moving anything, when its old place is taken) and make again the folders they removed, then remove the folders they made if they are empty. A folder that merged into one of the same name already there (Skip) stays where it went, since what is in that folder now is both, and the toast says so; attributes get their old values back; a symbolic link or junction is removed (never what it points to) and a hard link goes to the trash. They are library plans, so the library must be on (Settings → Library); the folders need not be in a library source. From a terminal: `keel plan merge|flatten|organize|attrs|link` ([api.md](api.md#cli)).

## Command line

```
keel [FOLDER] [--new-window] [--profile NAME] [--search QUERY] [--check-update] [--here]
```

| Flag | Meaning |
| --- | --- |
| `FOLDER` | Open this folder (a file opens its folder with the file selected). Relative paths are resolved against where you ran `keel` |
| `--new-window` | Start a separate window even if Keel is already running |
| `--profile NAME` | Use the profile `NAME` ([Profiles](#profiles)) |
| `--search QUERY` | Open a search tab with this query (in FOLDER when given) |
| `--check-update` | Ask GitHub once whether a newer release is out, print the answer and exit ([Updates](../README.md#updates)) |
| `--here` | Linux build inside WSL: open FOLDER in this Linux window instead of the Windows Keel (also `KEEL_WSL_NATIVE=1`; see [WSL](wsl.md#keel-inside-wsl)) |
| `--version`, `--help` | Print and exit; `--version` also prints whether this binary is signed |

Inside a WSL distribution, the Linux build hands FOLDER to the Windows `keel.exe` (`/mnt/c/...` becomes `C:\...`, other paths `\\wsl.localhost\<distribution>\...`), and `keel.exe .` started there opens the distribution's folder: [WSL](wsl.md#keel-inside-wsl).

By default there is one Keel per user and profile: a second `keel` hands its folder or search to the running window over a private pipe or socket that only your user can reach, brings it forward and exits. Turn this off with "Reuse the running window" in Settings → General. Settings → General also has the **global hotkeys**, which work from any app: **Global hotkey** (default Ctrl+Shift+Alt+K) brings Keel forward, **New window hotkey** opens another Keel window on the same profile (as `keel --new-window`), and **Toggle window hotkey** brings Keel forward, or minimizes it when it is already in front (both off by default). Empty disables one. Each needs Ctrl, Alt or the Windows/Command key besides Shift, and Ctrl+Alt alone is avoided because it is AltGr on many layouts. **New window** is also in the command palette.

## Profiles

A profile is a separate settings file and session (tabs, views, drop zone). Settings → Profiles creates one (a copy of the current settings), renames, deletes (to the OS trash) and switches; the command palette has "Switch profile: <name>". `keel --profile <name>` starts in a profile. Files: `<config>/profiles/<name>/config.toml`; the session is in the cache folder (`profiles/<name>/session.json`, or `session.json` for `default`).

## Appearance

Settings → **Appearance** sets how the window is drawn; the color theme (below) sets its colors on top.

- **Style**: **Finder-style** or **Classic**. Finder-style uses a Finder-like spacing scale (4 px base: 2, 4, 6, 8, 12, 16, 20, 24), rounder corners (6 on controls, 5 on selections, 10 on dialogs), 22 px list rows, 24 px sidebar rows and one type scale: 11 px small text, 12 px monospace, 13 px body text and buttons, 15 px headings. On the built-in dark and light themes it also uses the Finder colors: a white or near-black content area, a gray window background, a selection tinted with the accent, and secondary text kept at 4.5:1. Any other theme keeps its own colors in either style. Classic is Keel's original look: egui's spacing and type and the theme's colors, exactly as before. New profiles start in Finder-style; a profile whose `config.toml` existed before this setting keeps Classic until you switch. The change applies at once and is saved per profile (`style = "finder"` or `"classic"`).
- **Accent color**: **Automatic** (the system accent in Finder-style on the built-in dark and light themes, the theme's accent otherwise), **System**, **Theme** or **Custom** (a color picker; `accent = { custom = [r, g, b] }` in `config.toml`). The system accent is read once at start: on Windows the accent of Settings → Personalization → Colors, on macOS the one of System Settings → Appearance, on Linux GNOME's accent color (GNOME 47 and later); blue where there is none. An accent that would be hard to read is moved darker or lighter until links reach 4.5:1 on every background and the focus ring 3:1 (a test checks every macOS and GNOME accent, black, white and pale yellow).

The design tokens (spacing, radii, row heights, type scale, the eight Finder tag colors red, orange, yellow, green, blue, purple, gray and none, and the semantic colors for light and dark) are in [design.rs](../crates/keel-app/src/design.rs). Tests hold Classic to egui's defaults and check the contrast of all four token sets (Finder and Classic, light and dark): text 4.5:1, secondary text 4.5:1, tertiary text and the focus ring 3:1, text on the selection 4.5:1.

## Color themes

Settings → General → **Theme** lists the built-in themes (dark, light, Midnight, Paper, Dusk and High contrast), every `.toml` file in the `themes` folder of your config folder, under any name, and the VS Code color themes you installed. The choice applies at once and is saved per profile; **Theme** in the status bar still toggles dark and light. Each built-in keeps text, secondary text, links, warnings and errors at a contrast of at least 4.5:1 (a test checks it).

![The Paper theme](screenshots/theme-paper.png)

**Your own theme**: copy a built-in from [assets/themes](../assets/themes) into `<config>/themes/<name>.toml` and change its colours: `bg` (lists), `panel` (sidebar, bars, dialogs), `text`, `accent` (links, bars, the active tab), `selection`, `muted` (secondary text), and optionally `warn` and `error` (default: Keel's for a dark or a light theme). A file named `dark.toml` or `light.toml` overrides that built-in. A file that cannot be read falls back to dark.

**VS Code color themes**: type the extension id from the Visual Studio Marketplace (`publisher.name`) next to **Color themes** and press **Install from VS Code Marketplace…**. Keel downloads the package, shows its license and installs only after you press **I accept**. Every theme the package contributes is listed in the picker, and Keel switches to the first one; **Remove** deletes them again. Only the package's JSON is read: no code runs, nothing is fetched beyond the package, and the size caps are the icon themes'. Theme files may have comments and trailing commas and may `include` a parent theme, followed at most four levels and only inside the package. The colours Keel takes:

| Keel colour | VS Code key, the first one present |
| --- | --- |
| `bg` | `editor.background` |
| `panel` | `sideBarSectionHeader.background`, then `sideBar.background`, then `panel.background` |
| `text` | `foreground`, then `editor.foreground` |
| `accent` | `focusBorder`, then `button.background`, then `textLink.foreground` |
| `selection` | `list.activeSelectionBackground` |
| `muted` | `descriptionForeground`, then `disabledForeground` |

A theme with `uiTheme: "vs"` is light, any other dark; a missing key takes the built-in dark or light colour, and a colour with alpha is flattened onto `bg`. When the text, the secondary text or the accent is under 4.5:1 on `bg` or `panel`, Keel moves it toward legibility, and so it does with its warning and error colours; when the text is under 4.5:1 on the selection (selected rows draw all their text in the text colour), the selection is moved toward `bg`. The picker then says "adjusted for contrast". Each installed theme is a Keel theme file, `<config>/themes/<publisher.name>~<n>.toml`, whose first line marks it as Keel's: **Remove** and a reinstall touch only those files, never a file of your own with such a name. Keel does not bundle or redistribute Marketplace themes.

## Folder colors and icons

A folder in a library source can have a color and an icon of its own. Right-click it (in any view, a `library://` tab included) and choose:

- **Folder color**: one of the tag palette's colors (Red, Orange, Yellow, Green, Teal, Blue, Purple, Pink, Gray), or **Clear**.
- **Folder icon**: one of 40 common emoji, **Type an emoji…** for any other (one emoji: Win+. on Windows or Ctrl+Cmd+Space on macOS opens the system's emoji panel; `theme:<name>` also works there), **Icon theme** (when an [icon theme](#icon-themes) is installed: its own icons for folders named `src`, `docs`, `music` and so on, for any folder), or **Clear**.

The color tints the folder's icon in every view (Details, Grid, Columns, the media view) and puts a dot after its name in Details; an emoji takes the place of the folder icon, drawn in the folder's color when it has one. The sidebar's library section shows a styled source's icon next to its name, and Ctrl+P shows a styled folder's icon in its results. Screen readers hear "Folder color: Red" and "Folder icon: 📷".

Styles are kept in the library, never in the folder itself: the folder is not touched, and another program or file manager does not see them. They follow the folder when it is renamed or moved within its source (the library index records the move), they go when the folder is deleted or its source is removed from the library, and they are in a [library backup](library.md#backup-and-restore). With library sync on, they reach your other devices like tags kept by place: on the other device the style shows on this folder in its `node://` source of this device's folder ([Devices](devices.md)).

A folder outside every library source cannot be styled: its menu says **Add the folder's drive to the library to style it** (Library → **Add source…**). With the library off it says how to turn it on. `library.folder_style` and `library.set_folder_style` read and set styles through the API ([api.md](api.md)).

## Icon themes

Settings → Icons lists the built-in icons and any installed theme, previews them and switches at runtime. To install one, type the extension id from the Visual Studio Marketplace (`publisher.name`, for example `PKief.material-icon-theme`) and press **Install from VS Code Marketplace…**. Keel downloads the package, shows its license and installs only after you press **I accept**. Themes are unpacked into your own config folder; Keel does not bundle or redistribute them.

Only SVG and PNG icons are used. An SVG that references anything outside itself (an `<image>`, `<script>` or `<foreignObject>`, an external `href` or `url()`, CSS `@import`, entities) is refused, both at install time and when the theme loads, and the number skipped is reported. This stops a theme from making Keel read local files or network shares. A theme holds at most 10,000 icons and 64 MB.
