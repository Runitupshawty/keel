//! The **Permissions** tab: who owns each folder of a scan and who may do what there.
//!
//! - **Windows:** the owner and the DACL's entries (allow and deny; explicit or inherited,
//!   what they apply to), protected DACLs ("broken inheritance") and write grants to
//!   everyone, authenticated users, all users or anonymous flagged. Read with
//!   `READ_CONTROL` only, without recall and without following a final link, so nothing is
//!   hydrated or opened for data. Entries are grants as stored, not the effective access of
//!   a user (deny entries and group membership are not evaluated). Object and callback
//!   entries are listed as not decoded.
//! - **Unix:** owner, group and mode bits; world-writable flagged (POSIX ACLs and macOS
//!   ACLs are not read).

use super::analysis::csv_cell;
use super::{NodeId, NodeKind, ScanHandle, Tree};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// One entry of an access list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The account's name when it resolves, else its SID.
    pub trustee: String,
    pub sid: String,
    /// "Allow" or "Deny" (or what an entry not decoded is).
    pub kind: String,
    /// A summary of the mask: Full control, Modify, Read & execute, Read, Write, Special.
    pub access: String,
    pub mask: u32,
    /// Inherited from a parent folder (else explicit).
    pub inherited: bool,
    /// What it applies to: "This folder", "Subfolders", "Files", combined with " + ".
    pub applies_to: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Permissions {
    /// The owner's name when it resolves, else its SID or user id.
    pub owner: String,
    /// The owner's SID (Windows) or user id (Unix).
    pub owner_id: String,
    /// Unix: the group's name or id.
    pub group: Option<String>,
    /// Unix: the permission bits (with setuid, setgid and sticky).
    pub mode: Option<u32>,
    /// Windows: the folder does not inherit from its parent ("broken inheritance").
    pub protected: bool,
    /// Writable by everyone: a Unix `o+w` bit, or a Windows write grant to Everyone,
    /// Authenticated Users, Users or Anonymous (or no DACL at all).
    pub world_writable: bool,
    /// Every entry was understood.
    pub fully_decoded: bool,
    pub entries: Vec<Entry>,
}

impl Permissions {
    /// Entries set on the item itself.
    pub fn explicit(&self) -> usize {
        self.entries.iter().filter(|e| !e.inherited).count()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Row {
    pub node: NodeId,
    pub path: PathBuf,
    pub permissions: Option<Permissions>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// In scan order.
    pub rows: Vec<Row>,
    pub cancelled: bool,
}

impl Report {
    /// Each item's owner, for the search tab.
    pub fn owners(&self) -> HashMap<NodeId, String> {
        (self.rows.iter())
            .filter_map(|r| {
                let p = r.permissions.as_ref()?;
                let owner = if p.owner == p.owner_id {
                    p.owner.clone()
                } else {
                    format!("{} ({})", p.owner, p.owner_id)
                };
                Some((r.node, owner))
            })
            .collect()
    }

    /// Rows with a protected DACL, world-writable, entries not decoded, or errors.
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for r in &self.rows {
            match &r.permissions {
                Some(p) => {
                    c.protected += u64::from(p.protected);
                    c.world_writable += u64::from(p.world_writable);
                    c.explicit += u64::from(p.explicit() > 0);
                }
                None => c.errors += 1,
            }
        }
        c
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub protected: u64,
    pub world_writable: u64,
    /// Items with at least one explicit entry.
    pub explicit: u64,
    pub errors: u64,
}

/// Reads the permissions of the folders of `tree` (and its files too with `files`: the
/// owners the search tab filters by), several at a time. Progress goes to `handle` (items
/// as `files`); a cancel returns the rows read so far.
pub fn scan(tree: &Tree, files: bool, handle: &ScanHandle) -> Report {
    handle.start();
    let items: Vec<(NodeId, PathBuf)> = (tree.nodes())
        .filter(|(_, n)| match n.kind {
            NodeKind::Root | NodeKind::Dir => true,
            NodeKind::File | NodeKind::Link => files,
            _ => false,
        })
        .filter_map(|(id, _)| Some((id, tree.path(id)?)))
        .collect();
    let names = Names::default();
    let rows: Vec<Row> = (items.into_par_iter())
        .filter_map(|(node, path)| {
            if !handle.checkpoint() {
                return None;
            }
            handle.at(&path);
            let (permissions, error) = match read(&path, &names) {
                Ok(p) => (Some(p), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            };
            handle.add(1, 0, 0);
            Some(Row {
                node,
                path,
                permissions,
                error,
            })
        })
        .collect();
    let out = Report {
        rows,
        cancelled: handle.is_cancelled(),
    };
    handle.finish();
    out
}

/// The permissions of one item (a final link itself, not its target).
pub fn inspect(path: &Path) -> anyhow::Result<Permissions> {
    read(path, &Names::default())
}

/// Account names by SID or id, looked up once per scan.
#[derive(Default)]
struct Names(Mutex<HashMap<String, String>>);

impl Names {
    fn get(&self, id: &str, lookup: impl FnOnce() -> Option<String>) -> String {
        if let Some(n) = self.0.lock().get(id) {
            return n.clone();
        }
        let name = lookup().unwrap_or_else(|| id.to_owned());
        self.0.lock().insert(id.to_owned(), name.clone());
        name
    }
}

#[cfg(unix)]
fn read(path: &Path, names: &Names) -> anyhow::Result<Permissions> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    let mut p = from_mode(m.uid(), m.gid(), m.mode());
    p.owner = names.get(&format!("u{}", m.uid()), || unix::user(m.uid()));
    p.group = Some(names.get(&format!("g{}", m.gid()), || unix::group(m.gid())));
    // A link's own bits mean nothing (they are always 777 on Linux).
    if m.file_type().is_symlink() {
        p.world_writable = false;
    }
    Ok(p)
}

#[cfg(any(unix, test))]
pub(crate) fn from_mode(uid: u32, gid: u32, mode: u32) -> Permissions {
    Permissions {
        owner: uid.to_string(),
        owner_id: uid.to_string(),
        group: Some(gid.to_string()),
        mode: Some(mode & 0o7777),
        world_writable: mode & 0o002 != 0,
        fully_decoded: true,
        ..Permissions::default()
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::CStr;

    pub fn user(uid: u32) -> Option<String> {
        let mut buf = vec![0 as libc::c_char; 4096];
        // SAFETY: zeroed passwd is a valid out-parameter; `buf` outlives the result's
        // string pointers, which are copied before it drops.
        unsafe {
            let mut pwd: libc::passwd = std::mem::zeroed();
            let mut out = std::ptr::null_mut();
            let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut out);
            if rc != 0 || out.is_null() || pwd.pw_name.is_null() {
                return None;
            }
            Some(CStr::from_ptr(pwd.pw_name).to_string_lossy().into_owned())
        }
    }

    pub fn group(gid: u32) -> Option<String> {
        let mut buf = vec![0 as libc::c_char; 4096];
        // SAFETY: as `user`.
        unsafe {
            let mut grp: libc::group = std::mem::zeroed();
            let mut out = std::ptr::null_mut();
            let rc = libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut out);
            if rc != 0 || out.is_null() || grp.gr_name.is_null() {
                return None;
            }
            Some(CStr::from_ptr(grp.gr_name).to_string_lossy().into_owned())
        }
    }
}

/// Mask bits that let a holder change a file or folder's content, names or security.
#[cfg(any(windows, test))]
const WRITE_BITS: u32 = 0x2 // FILE_WRITE_DATA / FILE_ADD_FILE
    | 0x4 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
    | 0x40 // FILE_DELETE_CHILD
    | 0x1_0000 // DELETE
    | 0x4_0000 // WRITE_DAC
    | 0x8_0000 // WRITE_OWNER
    | 0x4000_0000 // GENERIC_WRITE
    | 0x1000_0000; // GENERIC_ALL

/// Everyone, Anonymous, Authenticated Users, Users.
#[cfg(any(windows, test))]
const BROAD: [&str; 4] = ["S-1-1-0", "S-1-5-7", "S-1-5-11", "S-1-5-32-545"];

/// What a Windows access mask amounts to, as Explorer's Security tab names it.
#[cfg(any(windows, test))]
pub(crate) fn access_text(mask: u32) -> String {
    const GENERIC_ALL: u32 = 0x1000_0000;
    const FULL: u32 = 0x1F_01FF;
    const MODIFY: u32 = 0x13_01BF;
    const READ_EXECUTE: u32 = 0x12_00A9;
    const READ: u32 = 0x12_0089;
    const WRITE: u32 = 0x10_0116;
    let has = |m: u32| mask & m == m;
    if mask & GENERIC_ALL != 0 || has(FULL) {
        "Full control".into()
    } else if has(MODIFY) {
        "Modify".into()
    } else if has(READ_EXECUTE) && !has(WRITE) {
        "Read & execute".into()
    } else if has(READ) && !has(WRITE) {
        "Read".into()
    } else if has(WRITE) && !has(READ) {
        "Write".into()
    } else {
        format!("Special ({mask:#x})")
    }
}

/// What an inheritable entry's flags make it apply to (a file's entries apply to it).
#[cfg(any(windows, test))]
pub(crate) fn applies_text(flags: u8) -> String {
    const OBJECT_INHERIT: u8 = 0x1;
    const CONTAINER_INHERIT: u8 = 0x2;
    const INHERIT_ONLY: u8 = 0x8;
    let mut parts = Vec::new();
    if flags & INHERIT_ONLY == 0 {
        parts.push("This folder");
    }
    if flags & CONTAINER_INHERIT != 0 {
        parts.push("Subfolders");
    }
    if flags & OBJECT_INHERIT != 0 {
        parts.push("Files");
    }
    if parts.is_empty() {
        "Nothing".into()
    } else {
        parts.join(" + ")
    }
}

/// Whether an entry grants write access to a broad group.
#[cfg(any(windows, test))]
pub(crate) fn broad_write(allow: bool, sid: &str, mask: u32, flags: u8) -> bool {
    const INHERIT_ONLY: u8 = 0x8;
    allow && flags & INHERIT_ONLY == 0 && mask & WRITE_BITS != 0 && BROAD.contains(&sid)
}

#[cfg(windows)]
fn read(path: &Path, names: &Names) -> anyhow::Result<Permissions> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetAce, GetSecurityDescriptorControl, LookupAccountSidW, ACCESS_ALLOWED_ACE, ACE_HEADER,
        ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SE_DACL_PROTECTED, SID_NAME_USE,
    };

    // READ_CONTROL only; FILE_FLAG_BACKUP_SEMANTICS (folders), FILE_FLAG_OPEN_REPARSE_POINT,
    // FILE_FLAG_OPEN_NO_RECALL: no data access, no recall, a link itself.
    let file = std::fs::OpenOptions::new()
        .access_mode(0x0002_0000)
        .share_mode(7)
        .custom_flags(0x0200_0000 | 0x0020_0000 | 0x0010_0000)
        .open(path)?;

    struct Local(*mut std::ffi::c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            // SAFETY: memory LocalAlloc'ed by the API that returned it, freed once.
            unsafe {
                let _ = LocalFree(HLOCAL(self.0));
            }
        }
    }

    /// SAFETY: `sid` points to a valid SID.
    unsafe fn sid_text(sid: PSID) -> anyhow::Result<String> {
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(sid, &mut text)?;
        let _free = Local(text.0.cast());
        Ok(text.to_string()?)
    }

    /// SAFETY: `sid` points to a valid SID.
    unsafe fn account(sid: PSID) -> Option<String> {
        let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
        let (mut n, mut d) = (name.len() as u32, domain.len() as u32);
        let mut use_ = SID_NAME_USE::default();
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            PWSTR(name.as_mut_ptr()),
            &mut n,
            PWSTR(domain.as_mut_ptr()),
            &mut d,
            &mut use_,
        )
        .ok()?;
        let name = String::from_utf16_lossy(&name[..n as usize]);
        let domain = String::from_utf16_lossy(&domain[..d as usize]);
        Some(if domain.is_empty() {
            name
        } else {
            format!("{domain}\\{name}")
        })
    }

    // SAFETY: GetSecurityInfo allocates one descriptor (freed by `_sd`); the owner, DACL,
    // ACE and SID pointers all point inside it. GetAce checks each index; only the two
    // basic ACE layouts (allow, deny) are read through ACCESS_ALLOWED_ACE, after checking
    // the ACE is at least that big.
    unsafe {
        let mut owner = PSID::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let err = GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut sd),
        );
        anyhow::ensure!(err == ERROR_SUCCESS, std::io::Error::from_raw_os_error(err.0 as i32));
        let _sd = Local(sd.0);
        let (mut control, mut revision) = (0u16, 0u32);
        GetSecurityDescriptorControl(sd, &mut control, &mut revision)?;
        let mut p = Permissions {
            protected: control & SE_DACL_PROTECTED.0 != 0,
            fully_decoded: true,
            ..Permissions::default()
        };
        if !owner.0.is_null() {
            p.owner_id = sid_text(owner)?;
            p.owner = names.get(&p.owner_id, || account(owner));
        }
        if dacl.is_null() {
            // No DACL: everyone may do anything.
            p.world_writable = true;
            p.entries.push(Entry {
                trustee: "Everyone".into(),
                sid: "S-1-1-0".into(),
                kind: "Allow".into(),
                access: "Full control (no access list)".into(),
                mask: 0x1F_01FF,
                inherited: false,
                applies_to: "This folder".into(),
            });
            return Ok(p);
        }
        for i in 0..u32::from((*dacl).AceCount) {
            let mut ace = std::ptr::null_mut();
            GetAce(dacl, i, &mut ace)?;
            let header = &*(ace as *const ACE_HEADER);
            let flags = header.AceFlags;
            let inherited = flags & 0x10 != 0;
            let basic = matches!(header.AceType, 0 | 1)
                && usize::from(header.AceSize) >= std::mem::size_of::<ACCESS_ALLOWED_ACE>();
            if !basic {
                p.fully_decoded = false;
                p.entries.push(Entry {
                    trustee: "Not decoded".into(),
                    sid: String::new(),
                    kind: format!("Entry type {}", header.AceType),
                    access: String::new(),
                    mask: 0,
                    inherited,
                    applies_to: applies_text(flags),
                });
                continue;
            }
            let entry = &*(ace as *const ACCESS_ALLOWED_ACE);
            let sid_ptr = PSID(std::ptr::addr_of!(entry.SidStart).cast_mut().cast());
            let sid = sid_text(sid_ptr)?;
            let allow = header.AceType == 0;
            p.world_writable |= broad_write(allow, &sid, entry.Mask, flags);
            p.entries.push(Entry {
                trustee: names.get(&sid, || account(sid_ptr)),
                sid,
                kind: if allow { "Allow" } else { "Deny" }.into(),
                access: access_text(entry.Mask),
                mask: entry.Mask,
                inherited,
                applies_to: applies_text(flags),
            });
        }
        Ok(p)
    }
}

#[cfg(not(any(windows, unix)))]
fn read(_path: &Path, _names: &Names) -> anyhow::Result<Permissions> {
    anyhow::bail!("permissions are not read on this system")
}

/// The report as CSV: one line per entry (one line for an item without entries).
pub fn write_csv(report: &Report, mut w: impl Write) -> std::io::Result<()> {
    write!(
        w,
        "path,owner,owner_id,group,mode,inheritance,world_writable,fully_decoded,\
         trustee,sid,kind,access,mask,inherited,applies_to,error\r\n"
    )?;
    for row in &report.rows {
        let path = csv_cell(&row.path.to_string_lossy());
        let Some(p) = &row.permissions else {
            let error = row.error.as_deref().unwrap_or("not read");
            write!(w, "{path},,,,,,,,,,,,,,,{}\r\n", csv_cell(error))?;
            continue;
        };
        let head = format!(
            "{path},{},{},{},{},{},{},{}",
            csv_cell(&p.owner),
            csv_cell(&p.owner_id),
            csv_cell(p.group.as_deref().unwrap_or("")),
            p.mode.map(|m| format!("{m:04o}")).unwrap_or_default(),
            if p.protected { "protected" } else { "inherits" },
            p.world_writable,
            p.fully_decoded,
        );
        if p.entries.is_empty() {
            write!(w, "{head},,,,,,,,\r\n")?;
        }
        for e in &p.entries {
            write!(
                w,
                "{head},{},{},{},{},{:#x},{},{},\r\n",
                csv_cell(&e.trustee),
                csv_cell(&e.sid),
                csv_cell(&e.kind),
                csv_cell(&e.access),
                e.mask,
                if e.inherited { "inherited" } else { "explicit" },
                csv_cell(&e.applies_to),
            )?;
        }
    }
    Ok(())
}
