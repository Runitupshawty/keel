//! Task 85: system cleanups, launchers, and the glue for Delete permanently and hard-link
//! dedup (`permanent_delete`, `hardlink_dedup`).
//!
//! **System cleanups** (Windows): disable hibernation (`powercfg /h off`), analyze and
//! clean up the component store (`Dism /Online /Cleanup-Image /AnalyzeComponentStore`,
//! `/StartComponentCleanup`) and optimize a VHD/VHDX (`Optimize-VHD` when the Hyper-V
//! module is installed in System32, else a `diskpart` `compact vdisk` script). Each opens
//! a preview with its size and the exact commands, then a second confirmation, and runs
//! through a one-shot elevated helper: this executable started through the UAC prompt as
//! `keel --system-cleanup <task> --cleanup-report <file>` (as `keel --index-service`),
//! which builds those fixed commands again itself, runs them and writes their output to
//! the report (a new file directly in its temporary folder).
//!
//! What the elevated helper runs never comes from a place the unelevated user can change
//! after the confirmation: programs are started by their full System32 path (from the OS,
//! not the environment), the diskpart script is handed over on standard input (no script
//! file), PowerShell gets `PSModulePath` set to the system modules only, module
//! auto-loading off and the Hyper-V module imported by its full System32 path, and .NET
//! profiler variables are removed. None of it is on a key; Keel's tests build the commands
//! and never run them.
//!
//! **Launchers:** Disk Cleanup, Installed apps, Programs and Features and Storage settings
//! on Windows; Storage settings on macOS; none on Linux (the menus are hidden).

use crate::dialogs::Dialog;
use crate::hardlink_dedup::{self, DedupPlan};
use crate::keys::Action;
use crate::permanent_delete::{self, Measured};
use crate::state::{AppState, Msg};
use crate::worker;
use humansize::{format_size, BINARY, DECIMAL};
use keel_vfs::VPath;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// The commands (an `Action::System`).
#[derive(Clone, Debug, PartialEq)]
pub enum SysCmd {
    /// Measure the targets, then the typed confirmation.
    DeletePermanently,
    /// The typed confirmation was given for exactly this.
    DeletePermanentlyNow(Box<Measured>),
    /// Duplicate finder: plan replacing the shown duplicates with hard links.
    DedupPreview,
    /// The preview was confirmed.
    DedupRun(Box<DedupPlan>),
    /// Open a system tool; `drive` for Disk Cleanup ("C:").
    Launch(Launcher, Option<String>),
    /// Open the preview of a system cleanup.
    Preview(Task),
    /// Optimize VHD/VHDX…: pick the file, then its preview.
    PickVhd,
    /// Confirmed twice: run it elevated.
    Run(Task),
}

/// Worker answers (a `Msg::System`).
pub enum SysMsg {
    Measured(Result<Measured, String>),
    DedupPlanned(DedupPlan),
    /// A preview, for the request with that id (see [`TaskDialog::request`]).
    Info(u64, Result<TaskInfo, String>),
    VhdPicked(PathBuf),
    /// A system cleanup finished: what to tell (and for an analysis, its report).
    Done {
        task: Task,
        result: Result<String, String>,
        output: String,
    },
}

/// The dialogs (a `Dialog::System`).
pub enum SysDialog {
    DeletePermanently(permanent_delete::Confirm),
    Dedup(hardlink_dedup::Preview),
    Task(TaskDialog),
    /// A finished cleanup's output (the component store analysis).
    Report {
        title: String,
        text: String,
    },
}

// ---- system cleanups ------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Task {
    Hibernation,
    DismAnalyze,
    DismCleanup,
    /// `powershell`: through `Optimize-VHD` (Hyper-V module), else `diskpart`.
    OptimizeVhd {
        path: PathBuf,
        powershell: bool,
    },
}

impl Task {
    pub fn title(&self) -> String {
        match self {
            Task::Hibernation => "Disable hibernation".into(),
            Task::DismAnalyze => "Analyze the component store".into(),
            Task::DismCleanup => "Clean up the component store".into(),
            Task::OptimizeVhd { path, .. } => format!(
                "Optimize {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }

    /// What it does and what it costs.
    pub fn about(&self) -> &'static str {
        match self {
            Task::Hibernation => {
                "Turns hibernation off and deletes hiberfil.sys. Hibernate and Fast Startup \
                 stop working until you run \"powercfg /h on\" as administrator."
            }
            Task::DismAnalyze => {
                "Reads the Windows component store (WinSxS) and reports its size and what a \
                 cleanup would free. Changes nothing; takes a few minutes."
            }
            Task::DismCleanup => {
                "Removes superseded versions of Windows components. Updates installed before \
                 may no longer be uninstallable. Takes several minutes."
            }
            Task::OptimizeVhd { .. } => {
                "Attaches the virtual disk read-only, gives the space of deleted files back to \
                 this drive, and detaches it. The disk must not be in use (shut down the VM \
                 or detach it first)."
            }
        }
    }

    /// The helper's `--system-cleanup` argument.
    pub fn to_arg(&self) -> String {
        match self {
            Task::Hibernation => "hibernate-off".into(),
            Task::DismAnalyze => "dism-analyze".into(),
            Task::DismCleanup => "dism-cleanup".into(),
            Task::OptimizeVhd { path, powershell } => format!(
                "{}:{}",
                if *powershell {
                    "vhd-ps"
                } else {
                    "vhd-diskpart"
                },
                path.display()
            ),
        }
    }

    /// Reads [`Task::to_arg`]; a virtual disk must be an absolute `.vhd` / `.vhdx` path.
    pub fn from_arg(arg: &str) -> Result<Task, String> {
        let vhd = |path: &str, powershell| {
            let path = PathBuf::from(path);
            check_vhd(&path)?;
            Ok(Task::OptimizeVhd { path, powershell })
        };
        match arg {
            "hibernate-off" => Ok(Task::Hibernation),
            "dism-analyze" => Ok(Task::DismAnalyze),
            "dism-cleanup" => Ok(Task::DismCleanup),
            _ => match arg.split_once(':') {
                Some(("vhd-ps", path)) => vhd(path, true),
                Some(("vhd-diskpart", path)) => vhd(path, false),
                _ => Err(format!("unknown system cleanup: {arg}")),
            },
        }
    }
}

/// Why `path` is not a virtual disk Keel optimizes (shape only; the disk is not read).
pub fn check_vhd(path: &Path) -> Result<(), String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    if !matches!(ext.as_deref(), Some("vhd" | "vhdx")) {
        return Err(format!("{} is not a .vhd or .vhdx file", path.display()));
    }
    if !path.is_absolute() {
        return Err(format!("{} is not a full path", path.display()));
    }
    let text = path.to_string_lossy();
    if text.contains(['"', '\n', '\r']) {
        return Err(format!(
            "{} has characters a script cannot quote",
            path.display()
        ));
    }
    Ok(())
}

/// One command a cleanup runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub program: String,
    pub args: Vec<String>,
    /// Environment set for it (the VHD path for PowerShell: never quoted into a script).
    pub env: Vec<(String, String)>,
    /// A script given on standard input (diskpart's commands; never a file).
    pub stdin: Option<String>,
}

impl Step {
    fn new(program: String, args: &[&str]) -> Step {
        Step {
            program,
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: Vec::new(),
            stdin: None,
        }
    }

    /// The command line as the preview shows it.
    pub fn shown(&self) -> String {
        let mut line = self.program.clone();
        for a in &self.args {
            line.push(' ');
            if a.contains(' ') {
                line += &format!("\"{a}\"");
            } else {
                line += a;
            }
        }
        if self.stdin.is_some() {
            line += " < script";
        }
        for (k, v) in &self.env {
            line += &format!("  ({k}={v})");
        }
        if let Some(s) = &self.stdin {
            for l in s.lines() {
                line += &format!("\n    {l}");
            }
        }
        line
    }
}

/// The PowerShell for Optimize-VHD: attach read-only, optimize fully, always detach. The
/// path comes from `KEEL_VHD` and the Hyper-V module's manifest (under System32) from
/// `KEEL_HYPERV`, so no quoting is involved; with auto-loading off and the commands named
/// with their module, nothing is loaded from a module folder the user can write.
pub const OPTIMIZE_VHD_PS: &str = "$PSModuleAutoLoadingPreference = 'None'; \
     $ErrorActionPreference = 'Stop'; \
     Import-Module -Name $env:KEEL_HYPERV; \
     Hyper-V\\Mount-VHD -Path $env:KEEL_VHD -ReadOnly -NoDriveLetter; \
     try { Hyper-V\\Optimize-VHD -Path $env:KEEL_VHD -Mode Full } \
     finally { Hyper-V\\Dismount-VHD -Path $env:KEEL_VHD }";

/// Where the programs a cleanup runs are: from the OS, never from the environment (which
/// the unelevated user controls).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysDirs {
    /// `C:\Windows\System32`.
    pub system32: String,
    /// The Hyper-V PowerShell module's manifest under System32, when it is installed.
    pub hyperv: Option<String>,
}

impl SysDirs {
    /// This system's (reads the disk for the Hyper-V module).
    pub fn here() -> SysDirs {
        let system32 = system32();
        let hyperv = hyperv_manifest(Path::new(&system32));
        SysDirs { system32, hyperv }
    }

    /// `name` in System32.
    fn exe(&self, name: &str) -> String {
        format!("{}\\{name}", self.system32)
    }

    /// Windows PowerShell's folder (`$PSHOME`).
    fn ps_home(&self) -> String {
        self.exe("WindowsPowerShell\\v1.0")
    }
}

/// System32, from GetSystemDirectoryW.
#[cfg(windows)]
fn system32() -> String {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }
    let mut buf = [0u16; 512];
    // SAFETY: the buffer and its length in characters.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return r"C:\Windows\System32".into();
    }
    String::from_utf16_lossy(&buf[..n])
}

#[cfg(not(windows))]
fn system32() -> String {
    r"C:\Windows\System32".into()
}

/// The Hyper-V module's manifest in System32's module folder: `Hyper-V\Hyper-V.psd1` or
/// the newest `Hyper-V\<version>\Hyper-V.psd1`.
pub fn hyperv_manifest(system32: &Path) -> Option<String> {
    let dir = system32
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("Modules")
        .join("Hyper-V");
    let direct = dir.join("Hyper-V.psd1");
    if direct.is_file() {
        return Some(direct.to_string_lossy().into_owned());
    }
    let version =
        |name: &str| -> Option<Vec<u32>> { name.split('.').map(|n| n.parse().ok()).collect() };
    let newest = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let v = version(&e.file_name().to_string_lossy())?;
            let psd1 = e.path().join("Hyper-V.psd1");
            psd1.is_file().then_some((v, psd1))
        })
        .max_by(|a, b| a.0.cmp(&b.0))?;
    Some(newest.1.to_string_lossy().into_owned())
}

/// .NET and PowerShell settings a child must not take from the user's environment: a
/// profiler DLL would be loaded into the elevated process.
fn unsafe_env(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.starts_with("COR_")
        || n.starts_with("CORECLR_")
        || n.starts_with("COMPLUS_")
        || n.starts_with("DOTNET_")
        || n == "PSMODULEPATH"
}

/// The commands `task` runs, in order (built here, and again by the elevated helper, which
/// runs them).
pub fn steps(task: &Task) -> Result<Vec<Step>, String> {
    steps_in(task, &SysDirs::here())
}

/// [`steps`] with the system's folders given.
pub fn steps_in(task: &Task, dirs: &SysDirs) -> Result<Vec<Step>, String> {
    Ok(match task {
        Task::Hibernation => vec![Step::new(dirs.exe("powercfg.exe"), &["/h", "off"])],
        Task::DismAnalyze => vec![Step::new(
            dirs.exe("Dism.exe"),
            &[
                "/Online",
                "/Cleanup-Image",
                "/AnalyzeComponentStore",
                "/English",
            ],
        )],
        Task::DismCleanup => vec![Step::new(
            dirs.exe("Dism.exe"),
            &[
                "/Online",
                "/Cleanup-Image",
                "/StartComponentCleanup",
                "/English",
            ],
        )],
        Task::OptimizeVhd { path, powershell } => {
            check_vhd(path)?;
            let shown = path.display().to_string();
            if *powershell {
                let hyperv = dirs.hyperv.clone().ok_or_else(|| {
                    "Optimize-VHD is not installed (no Hyper-V module in System32)".to_owned()
                })?;
                let mut s = Step::new(
                    format!("{}\\powershell.exe", dirs.ps_home()),
                    &["-NoProfile", "-NonInteractive", "-Command", OPTIMIZE_VHD_PS],
                );
                s.env.push((
                    "PSModulePath".into(),
                    format!("{}\\Modules", dirs.ps_home()),
                ));
                s.env.push(("KEEL_HYPERV".into(), hyperv));
                s.env.push(("KEEL_VHD".into(), shown));
                vec![s]
            } else {
                let mut s = Step::new(dirs.exe("diskpart.exe"), &[]);
                s.stdin = Some(format!(
                    "select vdisk file=\"{shown}\"\r\nattach vdisk readonly\r\n\
                     compact vdisk noerr\r\ndetach vdisk\r\nexit\r\n"
                ));
                vec![s]
            }
        }
    })
}

/// The elevated helper's arguments (`keel_search::run_elevated` passes them verbatim).
pub fn helper_args(task: &Task, report: &Path) -> String {
    format!(
        "--system-cleanup \"{}\" --cleanup-report \"{}\"",
        task.to_arg(),
        report.display()
    )
}

/// The report file the helper writes: named by Keel, directly in this process's
/// temporary folder, with no link or junction anywhere on the way (the elevated helper must
/// not create files in a folder a command line names).
pub fn report_ok(path: &Path) -> bool {
    report_ok_in(path, &std::env::temp_dir())
}

/// [`report_ok`] with the temporary folder given.
pub fn report_ok_in(path: &Path, temp: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let shape = path.is_absolute()
        && name.starts_with("keel-syscleanup-")
        && name.ends_with(".json")
        && !name.contains(['/', '\\', ':']);
    let Some(parent) = path.parent() else {
        return false;
    };
    shape
        && same_dir(parent, temp)
        && no_links_on_the_way(parent)
        && std::fs::symlink_metadata(path).is_err()
}

/// Whether `a` and `b` name one folder (compared resolved; links are refused elsewhere).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Whether no folder from `dir` up to its root is a link, junction or other reparse point.
fn no_links_on_the_way(dir: &Path) -> bool {
    dir.ancestors().all(|d| {
        d.parent().is_none()
            || std::fs::symlink_metadata(d).is_ok_and(|m| {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    m.is_dir() && m.file_attributes() & 0x400 == 0
                }
                #[cfg(not(windows))]
                {
                    m.is_dir() && !m.file_type().is_symlink()
                }
            })
    })
}

/// What the helper ran: the first failing command's exit code (0: all ran) and the
/// output of every command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub code: i32,
    pub output: String,
}

// ---- the component store report -------------------------------------------------------

/// `Dism /AnalyzeComponentStore /English`'s numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreReport {
    pub actual: Option<u64>,
    pub backups: Option<u64>,
    pub cache: Option<u64>,
    pub packages: Option<u32>,
    pub recommended: Option<bool>,
}

impl StoreReport {
    /// What a cleanup can free at most: backups and disabled features, cache.
    pub fn reclaimable(&self) -> Option<u64> {
        match (self.backups, self.cache) {
            (None, None) => None,
            (b, c) => Some(b.unwrap_or(0) + c.unwrap_or(0)),
        }
    }

    pub fn summary(&self) -> String {
        let size = |b: Option<u64>| b.map_or("?".into(), |b| format_size(b, BINARY));
        let mut text = format!(
            "Component store: {}; backups and disabled features {}, cache {}",
            size(self.actual),
            size(self.backups),
            size(self.cache)
        );
        if let Some(n) = self.packages {
            text += &format!("; {n} reclaimable packages");
        }
        match self.recommended {
            Some(true) => text += "; cleanup recommended",
            Some(false) => text += "; no cleanup needed",
            None => {}
        }
        text
    }
}

/// "8.25 GB" (DISM's sizes are binary: 1 GB = 1024³ bytes).
pub fn parse_size(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let n: f64 = parts.next()?.replace(',', "").parse().ok()?;
    let unit = match parts.next().map(str::to_ascii_uppercase).as_deref() {
        Some("BYTES" | "B") | None => 1.0,
        Some("KB") => 1024.0,
        Some("MB") => 1024.0 * 1024.0,
        Some("GB") => 1024.0 * 1024.0 * 1024.0,
        Some("TB") => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    (n >= 0.0).then(|| (n * unit).round() as u64)
}

pub fn parse_analysis(output: &str) -> StoreReport {
    let mut r = StoreReport::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(" : ") else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Actual Size of Component Store" => r.actual = parse_size(value),
            "Backups and Disabled Features" => r.backups = parse_size(value),
            "Cache and Temporary Data" => r.cache = parse_size(value),
            "Number of Reclaimable Packages" => r.packages = value.parse().ok(),
            "Component Store Cleanup Recommended" => {
                r.recommended = match value {
                    "Yes" => Some(true),
                    "No" => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    r
}

// ---- previews -------------------------------------------------------------------------

/// A preview: the task (with how a VHD is optimized decided), its size, a note and the
/// commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInfo {
    pub task: Task,
    pub size: Option<u64>,
    pub note: String,
    pub steps: Vec<Step>,
}

/// `hiberfil.sys` on the system drive, read from its folder listing (the file itself is
/// locked); None when there is none.
pub fn hiberfil_size() -> Option<u64> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let root = PathBuf::from(format!("{drive}\\"));
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .find(|e| e.file_name().eq_ignore_ascii_case("hiberfil.sys"))?
        .metadata()
        .ok()
        .map(|m| m.len())
}

/// Whether `Optimize-VHD` is installed: the Hyper-V module in System32's module folder
/// (where only administrators write; a module of that name elsewhere is never used).
fn has_optimize_vhd() -> bool {
    cfg!(windows) && SysDirs::here().hyperv.is_some()
}

/// The preview of `task` (worker: reads sizes, may ask PowerShell about Optimize-VHD).
/// `last`: the last component store analysis of this session.
pub fn gather(task: Task, last: Option<StoreReport>) -> Result<TaskInfo, String> {
    let (task, size, note) = match task {
        Task::Hibernation => {
            let size = hiberfil_size();
            let note = match size {
                Some(s) => format!("hiberfil.sys takes {}.", format_size(s, DECIMAL)),
                None => "There is no hiberfil.sys: hibernation looks off already.".into(),
            };
            (Task::Hibernation, size, note)
        }
        Task::DismAnalyze => (
            Task::DismAnalyze,
            None,
            "The size is known after the analysis.".into(),
        ),
        Task::DismCleanup => match last {
            Some(r) => (Task::DismCleanup, r.reclaimable(), r.summary()),
            None => (
                Task::DismCleanup,
                None,
                "Analyze the component store first to see what this frees.".into(),
            ),
        },
        Task::OptimizeVhd { path, .. } => {
            check_vhd(&path)?;
            let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if !meta.is_file() {
                return Err(format!("{} is not a file", path.display()));
            }
            let powershell = has_optimize_vhd();
            let note = format!(
                "{} is {} now. {}",
                path.display(),
                format_size(meta.len(), DECIMAL),
                if powershell {
                    "Optimize-VHD is installed (Hyper-V module in System32): it is used."
                } else {
                    "Optimize-VHD is not installed (no Hyper-V module in System32): diskpart \
                     compacts it."
                }
            );
            (
                Task::OptimizeVhd { path, powershell },
                Some(meta.len()),
                note,
            )
        }
    };
    let steps = steps(&task)?;
    Ok(TaskInfo {
        task,
        size,
        note,
        steps,
    })
}

// ---- the preview and the second confirmation --------------------------------------------

pub struct TaskDialog {
    pub task: Task,
    pub info: Option<Result<TaskInfo, String>>,
    /// The second confirmation is showing.
    pub confirming: bool,
    /// Which preview request this dialog waits for: a late answer to another (an earlier
    /// dialog's) never lands here.
    pub request: u64,
}

/// A button of the task dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Preview: go on to the second confirmation.
    Continue,
    /// Second confirmation: run it.
    Run,
    /// Second confirmation: back to the preview.
    Back,
    Cancel,
}

impl TaskDialog {
    pub fn new(task: Task) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            task,
            info: None,
            confirming: false,
            request: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Takes the preview answered for `request`: only this dialog's own, only once, and only
    /// one of the same kind of task (for the same disk) as asked; never while the second
    /// confirmation is showing. Whether it was taken.
    pub fn answer(&mut self, request: u64, info: Result<TaskInfo, String>) -> bool {
        if request != self.request || self.info.is_some() || self.confirming {
            return false;
        }
        if let Ok(i) = &info {
            let same = match (&self.task, &i.task) {
                (Task::OptimizeVhd { path: a, .. }, Task::OptimizeVhd { path: b, .. }) => a == b,
                (a, b) => a == b,
            };
            if !same {
                return false;
            }
            self.task = i.task.clone();
        }
        self.info = Some(info);
        true
    }

    /// A button: the action to run (only Run after Continue, with a ready preview), and
    /// whether the dialog closes.
    pub fn press(&mut self, b: Press) -> (Option<Action>, bool) {
        let ready = matches!(&self.info, Some(Ok(_)));
        match b {
            Press::Continue if ready && !self.confirming => {
                self.confirming = true;
                (None, false)
            }
            Press::Run if ready && self.confirming => {
                let task = match &self.info {
                    Some(Ok(i)) => i.task.clone(),
                    _ => self.task.clone(),
                };
                (Some(Action::System(SysCmd::Run(task))), true)
            }
            Press::Back => {
                self.confirming = false;
                (None, false)
            }
            Press::Cancel => (None, true),
            _ => (None, false),
        }
    }

    /// The second confirmation's question.
    pub fn confirm_text(&self) -> String {
        let (size, lines) = match &self.info {
            Some(Ok(i)) => (
                i.size,
                (i.steps.iter())
                    .map(|s| s.program.clone() + " " + &s.args.join(" "))
                    .collect::<Vec<_>>(),
            ),
            _ => (None, Vec::new()),
        };
        let what = match (&self.task, size) {
            (Task::Hibernation, Some(s)) => {
                format!(" hiberfil.sys ({}) is deleted.", format_size(s, DECIMAL))
            }
            (Task::DismCleanup, Some(s)) => {
                format!(" Up to {} is freed.", format_size(s, BINARY))
            }
            _ => String::new(),
        };
        let cmd = match lines.as_slice() {
            [one] if one.len() < 80 => format!(" ({})", one.trim()),
            _ => String::new(),
        };
        format!(
            "{}{cmd} as administrator now?{what} Windows asks for permission next.",
            self.task.title()
        )
    }
}

fn task_ui(ui: &mut egui::Ui, d: &mut TaskDialog, cancel: &mut bool) -> Option<Action> {
    let mut pressed = None;
    ui.strong(d.task.title());
    if d.confirming {
        ui.label(d.confirm_text());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let run = ui.button("Run as administrator");
            let back = ui.button("Back");
            let no = ui.button("Cancel");
            // Enter on the second confirmation cancels: running takes a click or Tab.
            crate::a11y::first_focus(&no);
            if run.clicked() {
                pressed = Some(Press::Run);
            } else if back.clicked() {
                pressed = Some(Press::Back);
            } else if no.clicked() {
                pressed = Some(Press::Cancel);
            }
        });
    } else {
        ui.label(d.task.about());
        ui.add_space(4.0);
        match &d.info {
            None => {
                ui.horizontal(|ui| {
                    crate::anim::spinner(ui);
                    ui.weak("Measuring…");
                });
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            Some(Ok(info)) => {
                ui.label(&info.note);
                ui.add_space(4.0);
                ui.weak("Runs as administrator:");
                for s in &info.steps {
                    ui.add(egui::Label::new(egui::RichText::new(s.shown()).monospace()).wrap());
                }
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let ready = matches!(&d.info, Some(Ok(_)));
            let go = ui.add_enabled(ready, egui::Button::new("Continue…"));
            let no = ui.button("Cancel");
            if ready {
                crate::a11y::first_focus(&go);
            }
            if go.clicked() {
                pressed = Some(Press::Continue);
            } else if no.clicked() {
                pressed = Some(Press::Cancel);
            }
        });
    }
    let (action, close) = pressed.map_or((None, false), |b| d.press(b));
    *cancel |= close && action.is_none();
    action
}

/// The body of a `Dialog::System` (inside `dialogs::show`'s modal).
pub fn dialog_ui(ui: &mut egui::Ui, d: &mut SysDialog, cancel: &mut bool) -> Option<Action> {
    match d {
        SysDialog::DeletePermanently(c) => permanent_delete::ui(ui, c, cancel),
        SysDialog::Dedup(p) => hardlink_dedup::ui(ui, p, cancel),
        SysDialog::Task(t) => task_ui(ui, t, cancel),
        SysDialog::Report { title, text } => {
            ui.strong(title.as_str());
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new(text.as_str()).monospace(),
                    ));
                });
            ui.add_space(8.0);
            let ok = ui.button("OK");
            crate::a11y::first_focus(&ok);
            *cancel |= ok.clicked();
            None
        }
    }
}

// ---- running (the elevated helper) ------------------------------------------------------

/// Runs `task`'s commands in this process (the elevated helper, or Keel already running
/// as administrator), building them again here: programs by their full System32 path, the
/// diskpart script on standard input, no .NET profiler or user module path inherited.
/// Stops at the first command that fails.
#[cfg(windows)]
pub fn run_task(task: &Task) -> anyhow::Result<Outcome> {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut out = Outcome::default();
    for step in steps(task).map_err(anyhow::Error::msg)? {
        let mut cmd = std::process::Command::new(&step.program);
        for (k, _) in std::env::vars_os() {
            if unsafe_env(&k.to_string_lossy()) {
                cmd.env_remove(k);
            }
        }
        cmd.args(&step.args)
            .envs(step.env.iter().map(|(k, v)| (k, v)))
            .stdin(if step.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("{}: {e}", step.program))?;
        if let (Some(text), Some(mut stdin)) = (&step.stdin, child.stdin.take()) {
            // A few lines: fits the pipe, and closing it ends diskpart's input.
            let fed = stdin.write_all(text.as_bytes());
            drop(stdin);
            if let Err(e) = fed {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("{}: {e}", step.program);
            }
        }
        let ran = child
            .wait_with_output()
            .map_err(|e| anyhow::anyhow!("{}: {e}", step.program))?;
        out.output += &String::from_utf8_lossy(&ran.stdout);
        out.output += &String::from_utf8_lossy(&ran.stderr);
        if !ran.status.success() {
            out.code = ran.status.code().unwrap_or(1);
            break;
        }
    }
    Ok(out)
}

#[cfg(windows)]
fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Runs `task` as administrator: in this process when it is elevated, else through the
/// UAC prompt as `keel --system-cleanup` and its report file. Blocks (a job).
#[cfg(windows)]
fn run_elevated(task: &Task) -> anyhow::Result<Outcome> {
    use anyhow::Context as _;
    if keel_search::is_elevated() {
        return run_task(task);
    }
    let report = std::env::temp_dir().join(format!(
        "keel-syscleanup-{}-{}.json",
        std::process::id(),
        nanos()
    ));
    let _ = std::fs::remove_file(&report);
    let exe = std::env::current_exe()?;
    let code = keel_search::run_elevated(&exe, &helper_args(task, &report))
        .context("starting the elevated helper (was the Windows prompt declined?)")?;
    let text = std::fs::read_to_string(&report);
    let _ = std::fs::remove_file(&report);
    let text = text.with_context(|| format!("the helper wrote no report (exit code {code})"))?;
    Ok(serde_json::from_str(&text)?)
}

/// Body of `keel --system-cleanup <task> --cleanup-report <file>` (elevated): runs the
/// task, writes the [`Outcome`] as JSON into a new report file. The exit code is the
/// first failing command's (0: done).
pub fn helper_main(task: &str, report: Option<&Path>) -> i32 {
    let task = match Task::from_arg(task) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("system cleanup: {e}");
            return 2;
        }
    };
    let report = report.filter(|r| report_ok(r));
    #[cfg(windows)]
    let outcome = run_task(&task).unwrap_or_else(|e| Outcome {
        code: 1,
        output: format!("{e:#}"),
    });
    #[cfg(not(windows))]
    let outcome = Outcome {
        code: 1,
        output: format!("{} is Windows only", task.title()),
    };
    if let Some(path) = report {
        use std::io::Write;
        // A new file only: never written through a link planted at that name.
        let written = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .and_then(|mut f| f.write_all(&serde_json::to_vec(&outcome).unwrap_or_default()));
        if let Err(e) = written {
            tracing::error!("system cleanup report {}: {e}", path.display());
        }
    }
    outcome.code
}

/// The job body: sizes before, the elevated run, sizes after; the toast text.
#[cfg(windows)]
fn run_job(task: &Task) -> (Result<String, String>, String) {
    let before = match task {
        Task::Hibernation => hiberfil_size(),
        Task::OptimizeVhd { path, .. } => std::fs::metadata(path).ok().map(|m| m.len()),
        _ => None,
    };
    let out = match run_elevated(task) {
        Ok(o) => o,
        Err(e) => return (Err(format!("{}: {e:#}", task.title())), String::new()),
    };
    let tail = |o: &str| {
        let lines: Vec<&str> = o.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(3)..].join(" ")
    };
    if out.code != 0 {
        let why = format!(
            "{} failed (exit code {}): {}",
            task.title(),
            out.code,
            tail(&out.output)
        );
        return (Err(why), out.output);
    }
    let text = match task {
        Task::Hibernation => match (before, hiberfil_size()) {
            (Some(b), None) => format!(
                "Hibernation is off: hiberfil.sys ({}) was deleted",
                format_size(b, DECIMAL)
            ),
            _ => "Hibernation is off".into(),
        },
        Task::DismAnalyze => parse_analysis(&out.output).summary(),
        Task::DismCleanup => "The component store was cleaned up".into(),
        Task::OptimizeVhd { path, .. } => {
            let after = std::fs::metadata(path).ok().map(|m| m.len());
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            match (before, after) {
                (Some(b), Some(a)) => format!(
                    "Optimized {name}: {} → {} ({} freed)",
                    format_size(b, DECIMAL),
                    format_size(a, DECIMAL),
                    format_size(b.saturating_sub(a), DECIMAL)
                ),
                _ => format!("Optimized {name}"),
            }
        }
    };
    (Ok(text), out.output)
}

// ---- launchers ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launcher {
    DiskCleanup,
    InstalledApps,
    ProgramsAndFeatures,
    StorageSettings,
}

impl Launcher {
    pub fn label(self) -> &'static str {
        match self {
            Launcher::DiskCleanup => "Disk Cleanup",
            Launcher::InstalledApps => "Installed apps",
            Launcher::ProgramsAndFeatures => "Programs and Features",
            Launcher::StorageSettings => "Storage settings",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

pub const OS: Os = if cfg!(windows) {
    Os::Windows
} else if cfg!(target_os = "macos") {
    Os::Mac
} else {
    Os::Linux
};

/// How a launcher opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// A URL for the system's handler (`ms-settings:`, `x-apple.systempreferences:`).
    Url(&'static str),
    Program(&'static str, Vec<String>),
}

/// The launchers `os` has (none on Linux: its menus are hidden).
pub fn launchers(os: Os) -> &'static [Launcher] {
    match os {
        Os::Windows => &[
            Launcher::DiskCleanup,
            Launcher::StorageSettings,
            Launcher::InstalledApps,
            Launcher::ProgramsAndFeatures,
        ],
        Os::Mac => &[Launcher::StorageSettings],
        Os::Linux => &[],
    }
}

/// What `l` runs on `os`; `drive` ("C:" or "C:\") picks Disk Cleanup's drive.
pub fn launch_for(os: Os, l: Launcher, drive: Option<&str>) -> Option<Launch> {
    match (os, l) {
        (Os::Windows, Launcher::DiskCleanup) => {
            let letter = drive.and_then(|d| {
                let b = d.as_bytes();
                (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
                    .then(|| char::from(b[0]).to_ascii_uppercase())
            });
            let args = letter.map_or_else(Vec::new, |c| vec!["/d".to_owned(), c.to_string()]);
            Some(Launch::Program("cleanmgr.exe", args))
        }
        (Os::Windows, Launcher::InstalledApps) => Some(Launch::Url("ms-settings:appsfeatures")),
        (Os::Windows, Launcher::ProgramsAndFeatures) => {
            Some(Launch::Program("control.exe", vec!["appwiz.cpl".into()]))
        }
        (Os::Windows, Launcher::StorageSettings) => Some(Launch::Url("ms-settings:storagesense")),
        (Os::Mac, Launcher::StorageSettings) => Some(Launch::Url(
            "x-apple.systempreferences:com.apple.settings.Storage",
        )),
        _ => None,
    }
}

fn launch(l: Launcher, drive: Option<&str>) -> std::io::Result<()> {
    match launch_for(OS, l, drive) {
        Some(Launch::Url(url)) => open::that_detached(url),
        Some(Launch::Program(exe, args)) => {
            std::process::Command::new(exe).args(args).spawn().map(drop)
        }
        None => Err(std::io::Error::other("not on this system")),
    }
}

/// Whether drives get a Clean up menu here.
pub const DRIVE_MENU: bool = cfg!(any(windows, target_os = "macos"));

/// Whether `drive` ("C:", "C:\") is the Windows system drive.
fn is_system_drive(drive: &str) -> bool {
    let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    drive
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(sys.trim_end_matches(['\\', '/']))
}

/// The sidebar's "Clean up" submenu for the drive `name` ("C:" on Windows, a mount point
/// elsewhere). Nothing on Linux.
pub fn drive_menu(ui: &mut egui::Ui, name: &str, out: &mut Vec<Action>) {
    if !DRIVE_MENU {
        return;
    }
    let mut item = |ui: &mut egui::Ui, text: &str, action: Action| {
        if ui.button(text).clicked() {
            out.push(action);
            ui.close_menu();
        }
    };
    ui.menu_button("Clean up", |ui| {
        for &l in launchers(OS) {
            item(
                ui,
                l.label(),
                Action::System(SysCmd::Launch(l, Some(name.into()))),
            );
        }
        if OS == Os::Windows {
            ui.separator();
            if is_system_drive(name) {
                item(
                    ui,
                    "Disable hibernation…",
                    Action::System(SysCmd::Preview(Task::Hibernation)),
                );
                item(
                    ui,
                    "Analyze the component store…",
                    Action::System(SysCmd::Preview(Task::DismAnalyze)),
                );
                item(
                    ui,
                    "Clean up the component store…",
                    Action::System(SysCmd::Preview(Task::DismCleanup)),
                );
            }
            item(ui, "Optimize VHD/VHDX…", Action::System(SysCmd::PickVhd));
        }
    });
}

/// The palette's entries: Delete permanently with a selection; the cleanups and launchers.
pub fn palette_items(selection: bool) -> Vec<crate::palette::Item> {
    let mut items: Vec<(String, Action)> = Vec::new();
    if selection {
        items.push((
            "Delete permanently…".into(),
            Action::System(SysCmd::DeletePermanently),
        ));
    }
    for &l in launchers(OS) {
        items.push((l.label().into(), Action::System(SysCmd::Launch(l, None))));
    }
    if OS == Os::Windows {
        for task in [Task::Hibernation, Task::DismAnalyze, Task::DismCleanup] {
            items.push((
                format!("{}…", task.title()),
                Action::System(SysCmd::Preview(task)),
            ));
        }
        items.push(("Optimize VHD/VHDX…".into(), Action::System(SysCmd::PickVhd)));
    }
    items
        .into_iter()
        .map(|(label, action)| crate::palette::Item {
            label,
            shortcut: String::new(),
            action,
        })
        .collect()
}

// ---- AppState glue --------------------------------------------------------------------

/// Where the last component store analysis is kept for the cleanup's preview.
fn store_id() -> egui::Id {
    egui::Id::new("keel-component-store")
}

impl AppState {
    fn system_dialog(&mut self, d: SysDialog) {
        if self.dialog.is_some() {
            return self.toasts.error("Another dialog is open");
        }
        self.dialog = Some(Dialog::System(Box::new(d)));
    }

    pub fn system_cmd(&mut self, p: usize, cmd: SysCmd) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        match cmd {
            SysCmd::DeletePermanently => {
                let paths = self.target_paths(p);
                if let Some(why) = permanent_delete::refusal(&paths) {
                    return self.toasts.error(why);
                }
                worker::spawn("keel-measure", move || {
                    let m = permanent_delete::measure(&paths, &AtomicBool::new(false));
                    let m = m.map_err(|e| format!("Nothing deleted: {e:#}"));
                    worker::send(&tx, &ctx, Msg::System(SysMsg::Measured(m)));
                });
            }
            SysCmd::DeletePermanentlyNow(m) => {
                self.undo.refuse(crate::state::PERMANENT);
                let title = format!("Deleting {} permanently", crate::jobs::items(m.paths.len()));
                self.jobs.spawn(title, tx, move |report, cancel| {
                    permanent_delete::run(&m, report, cancel)
                });
            }
            SysCmd::DedupPreview => {
                let groups = match self.library.dups.as_ref().and_then(|d| d.groups.as_ref()) {
                    Some(Ok(g)) => g.clone(),
                    _ => return self.toasts.error("Find duplicates first"),
                };
                worker::spawn("keel-dedup-plan", move || {
                    let plan = hardlink_dedup::plan(&groups);
                    worker::send(&tx, &ctx, Msg::System(SysMsg::DedupPlanned(plan)));
                });
            }
            SysCmd::DedupRun(plan) => {
                let n = plan.pairs.len();
                let title = format!("Replacing {} with hard links", crate::jobs::items(n));
                let (router, generation) = (self.router.clone(), self.undo.generation());
                let (utx, uctx) = (tx.clone(), ctx);
                self.jobs.spawn(title, tx, move |report, cancel| {
                    let (linked, failed) = hardlink_dedup::run(&plan, report, cancel);
                    let stat = crate::undo::stat_fn(&router);
                    let mut before = Vec::new();
                    let mut items = Vec::new();
                    for l in &linked {
                        let now = VPath::local(&l.path);
                        if let Some(stamp) = stat(&now) {
                            items.push(crate::undo::Item {
                                now: now.clone(),
                                was: None,
                                stamp: Some(stamp),
                            });
                            before.push((now, l.mtime));
                        }
                    }
                    let done = Ok((crate::undo::Op::Hardlink(before), items));
                    worker::send(&utx, &uctx, Msg::Undoable(generation, done));
                    let stopped = cancel.load(std::sync::atomic::Ordering::Relaxed);
                    match failed.first() {
                        None if stopped && linked.is_empty() => {
                            Err(anyhow::anyhow!("operation cancelled"))
                        }
                        None if stopped => Err(anyhow::anyhow!(
                            "Cancelled: {} of {n} replaced",
                            linked.len()
                        )),
                        None => Ok(()),
                        Some(e) => Err(anyhow::anyhow!(
                            "{} of {n} replaced; {} left as they were (first: {e})",
                            linked.len(),
                            failed.len()
                        )),
                    }
                });
            }
            SysCmd::Launch(l, drive) => {
                if let Err(e) = launch(l, drive.as_deref()) {
                    self.toasts
                        .error(format!("Could not open {}: {e}", l.label()));
                }
            }
            SysCmd::Preview(task) => {
                if OS != Os::Windows {
                    return self.toasts.error("System cleanups are Windows only");
                }
                if self.dialog.is_some() {
                    return self.toasts.error("Another dialog is open");
                }
                let last = ctx.data(|d| d.get_temp::<StoreReport>(store_id()));
                let dialog = TaskDialog::new(task.clone());
                let request = dialog.request;
                self.dialog = Some(Dialog::System(Box::new(SysDialog::Task(dialog))));
                worker::spawn("keel-cleanup-preview", move || {
                    let info = gather(task, last);
                    worker::send(&tx, &ctx, Msg::System(SysMsg::Info(request, info)));
                });
            }
            SysCmd::PickVhd => {
                worker::spawn("keel-pick", move || {
                    let picked = rfd::FileDialog::new()
                        .set_title("Optimize virtual disk")
                        .add_filter("Virtual disks", &["vhd", "vhdx"])
                        .pick_file();
                    if let Some(path) = picked {
                        worker::send(&tx, &ctx, Msg::System(SysMsg::VhdPicked(path)));
                    }
                });
            }
            #[cfg(windows)]
            SysCmd::Run(task) => {
                let title = format!("{} (administrator)", task.title());
                let (mtx, mctx) = (tx.clone(), ctx);
                self.jobs.spawn(title, tx, move |_, _| {
                    let (result, output) = run_job(&task);
                    let failed = result.as_ref().err().cloned();
                    let done = SysMsg::Done {
                        task,
                        result,
                        output,
                    };
                    worker::send(&mtx, &mctx, Msg::System(done));
                    failed.map_or(Ok(()), |e| Err(anyhow::anyhow!(e)))
                });
            }
            #[cfg(not(windows))]
            SysCmd::Run(_) => self.toasts.error("System cleanups are Windows only"),
        }
    }

    pub fn system_msg(&mut self, m: SysMsg) {
        match m {
            SysMsg::Measured(Err(e)) => self.toasts.error(e),
            SysMsg::Measured(Ok(m)) => self.system_dialog(SysDialog::DeletePermanently(
                permanent_delete::Confirm::new(m),
            )),
            SysMsg::DedupPlanned(plan) => {
                self.system_dialog(SysDialog::Dedup(hardlink_dedup::Preview { plan }))
            }
            SysMsg::Info(request, info) => {
                if let Some(Dialog::System(d)) = &mut self.dialog {
                    if let SysDialog::Task(t) = d.as_mut() {
                        t.answer(request, info);
                    }
                }
            }
            SysMsg::VhdPicked(path) => {
                if let Err(e) = check_vhd(&path) {
                    return self.toasts.error(e);
                }
                let p = self.active;
                self.system_cmd(
                    p,
                    SysCmd::Preview(Task::OptimizeVhd {
                        path,
                        powershell: false,
                    }),
                );
            }
            SysMsg::Done {
                task,
                result,
                output,
            } => match result {
                Err(_) => {} // the job row says why
                Ok(text) if task == Task::DismAnalyze => {
                    let report = parse_analysis(&output);
                    self.ctx.data_mut(|d| d.insert_temp(store_id(), report));
                    if self.dialog.is_none() {
                        self.dialog = Some(Dialog::System(Box::new(SysDialog::Report {
                            title: text,
                            text: output,
                        })));
                    } else {
                        self.toasts.info(text);
                    }
                }
                Ok(text) => self.toasts.info(text),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vhd() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"D:\VMs\it's a disk\Disk 1.vhdx"
        } else {
            "/vms/it's a disk/Disk 1.vhdx"
        })
    }

    /// A system with the Hyper-V module (or without).
    fn dirs(hyperv: bool) -> SysDirs {
        SysDirs {
            system32: r"C:\Windows\System32".into(),
            hyperv: hyperv.then(|| {
                r"C:\Windows\System32\WindowsPowerShell\v1.0\Modules\Hyper-V\2.0.0.0\Hyper-V.psd1"
                    .into()
            }),
        }
    }

    #[test]
    fn commands_for_each_cleanup() {
        let d = dirs(false);
        assert_eq!(
            steps_in(&Task::Hibernation, &d).unwrap(),
            [Step::new(
                r"C:\Windows\System32\powercfg.exe".into(),
                &["/h", "off"]
            )]
        );
        let analyze = &steps_in(&Task::DismAnalyze, &d).unwrap()[0];
        assert_eq!(analyze.program, r"C:\Windows\System32\Dism.exe");
        assert_eq!(
            analyze.args,
            [
                "/Online",
                "/Cleanup-Image",
                "/AnalyzeComponentStore",
                "/English"
            ]
        );
        let cleanup = &steps_in(&Task::DismCleanup, &d).unwrap()[0];
        assert_eq!(
            cleanup.args,
            [
                "/Online",
                "/Cleanup-Image",
                "/StartComponentCleanup",
                "/English"
            ]
        );
        assert!(!cleanup.args.iter().any(|a| a.contains("ResetBase")));
        // Every program by its full System32 path, never looked up on PATH.
        for task in [Task::Hibernation, Task::DismAnalyze, Task::DismCleanup] {
            for s in steps(&task).unwrap() {
                assert!(Path::new(&s.program).is_absolute(), "{}", s.program);
            }
        }
    }

    #[test]
    fn vhd_through_powershell_loads_nothing_from_user_module_folders() {
        let path = vhd();
        let s = steps_in(
            &Task::OptimizeVhd {
                path: path.clone(),
                powershell: true,
            },
            &dirs(true),
        )
        .unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].program,
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        );
        assert_eq!(
            s[0].args,
            ["-NoProfile", "-NonInteractive", "-Command", OPTIMIZE_VHD_PS]
        );
        // The path (with a quote and spaces) is never in the script text.
        assert!(!OPTIMIZE_VHD_PS.contains("Disk 1"));
        assert_eq!(
            s[0].env,
            [
                (
                    "PSModulePath".to_owned(),
                    r"C:\Windows\System32\WindowsPowerShell\v1.0\Modules".to_owned()
                ),
                ("KEEL_HYPERV".to_owned(), dirs(true).hyperv.unwrap()),
                ("KEEL_VHD".to_owned(), path.display().to_string()),
            ]
        );
        // No auto-loading; the module by its System32 manifest; module-qualified commands.
        assert!(OPTIMIZE_VHD_PS.starts_with("$PSModuleAutoLoadingPreference = 'None';"));
        assert!(OPTIMIZE_VHD_PS.contains("Import-Module -Name $env:KEEL_HYPERV;"));
        for cmd in ["Mount-VHD", "Optimize-VHD", "Dismount-VHD"] {
            assert_eq!(
                OPTIMIZE_VHD_PS.matches(cmd).count(),
                OPTIMIZE_VHD_PS.matches(&format!("Hyper-V\\{cmd}")).count(),
                "{cmd}"
            );
        }
        assert!(OPTIMIZE_VHD_PS.contains("-ReadOnly") && OPTIMIZE_VHD_PS.contains("finally"));
        assert!(OPTIMIZE_VHD_PS.contains("Optimize-VHD -Path $env:KEEL_VHD -Mode Full"));
        // No Hyper-V module in System32: the PowerShell route is refused (the elevated
        // helper builds the steps again itself and never trusts the preview's choice).
        let e = steps_in(
            &Task::OptimizeVhd {
                path,
                powershell: true,
            },
            &dirs(false),
        )
        .unwrap_err();
        assert!(e.contains("not installed"), "{e}");
    }

    #[test]
    fn vhd_through_diskpart_is_a_script_on_standard_input() {
        let path = vhd();
        let s = steps_in(
            &Task::OptimizeVhd {
                path: path.clone(),
                powershell: false,
            },
            &dirs(true),
        )
        .unwrap();
        assert_eq!(s[0].program, r"C:\Windows\System32\diskpart.exe");
        // No `/s <file>`: nothing in a folder the user can write.
        assert!(s[0].args.is_empty(), "{:?}", s[0].args);
        let temp = std::env::temp_dir().display().to_string();
        assert!(!s[0].shown().contains(&temp));
        let script = s[0].stdin.as_deref().unwrap();
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(
            lines,
            [
                format!("select vdisk file=\"{}\"", path.display()).as_str(),
                "attach vdisk readonly",
                "compact vdisk noerr",
                "detach vdisk",
                "exit",
            ]
        );
        assert!(s[0]
            .shown()
            .starts_with(r"C:\Windows\System32\diskpart.exe < script"));
    }

    #[test]
    fn the_hyperv_module_is_found_in_system32_only() {
        let root = std::env::temp_dir().join(format!("keel-sys32-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let modules = root.join(r"WindowsPowerShell/v1.0/Modules/Hyper-V");
        assert_eq!(hyperv_manifest(&root), None);
        for v in ["1.1", "2.0.0.0"] {
            std::fs::create_dir_all(modules.join(v)).unwrap();
            std::fs::write(modules.join(v).join("Hyper-V.psd1"), b"@{}").unwrap();
        }
        std::fs::create_dir_all(modules.join("notes")).unwrap();
        let found = hyperv_manifest(&root).unwrap();
        assert!(found.contains("2.0.0.0"), "{found}");
        std::fs::write(modules.join("Hyper-V.psd1"), b"@{}").unwrap();
        assert!(hyperv_manifest(&root).unwrap().ends_with("Hyper-V.psd1"));
        let _ = std::fs::remove_dir_all(&root);
        // What the elevated children never inherit.
        for bad in [
            "COR_ENABLE_PROFILING",
            "COR_PROFILER_PATH",
            "COMPlus_ETWEnabled",
            "DOTNET_STARTUP_HOOKS",
            "PSModulePath",
        ] {
            assert!(unsafe_env(bad), "{bad}");
        }
        for fine in ["Path", "SystemRoot", "TEMP", "KEEL_VHD"] {
            assert!(!unsafe_env(fine), "{fine}");
        }
    }

    #[test]
    fn the_report_goes_only_directly_into_the_temporary_folder() {
        let temp = std::env::temp_dir();
        let ok = temp.join(format!("keel-syscleanup-{}-1.json", std::process::id()));
        let _ = std::fs::remove_file(&ok);
        assert!(report_ok(&ok));
        // Another folder, even one the elevated helper could write.
        let sys = PathBuf::from(r"C:\Windows\System32\keel-syscleanup-1-2.json");
        assert!(!report_ok(&sys));
        let sub = temp.join(format!("keel-report-sub-{}", std::process::id()));
        std::fs::create_dir_all(&sub).unwrap();
        assert!(!report_ok(&sub.join("keel-syscleanup-1-2.json")));
        // Through `..` into another folder.
        let dotdot = sub.join("..").join("..").join("keel-syscleanup-1-2.json");
        assert!(!report_ok_in(&dotdot, &temp));
        // A file already there (planted) is never written through.
        std::fs::write(&ok, b"planted").unwrap();
        assert!(!report_ok(&ok));
        std::fs::remove_file(&ok).unwrap();
        let _ = std::fs::remove_dir_all(&sub);
        // A temporary folder reached through a link.
        #[cfg(unix)]
        {
            let real = temp.join(format!("keel-report-real-{}", std::process::id()));
            let link = temp.join(format!("keel-report-link-{}", std::process::id()));
            std::fs::create_dir_all(&real).unwrap();
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(!report_ok_in(&link.join("keel-syscleanup-1-2.json"), &real));
            let _ = std::fs::remove_file(&link);
            let _ = std::fs::remove_dir_all(&real);
        }
        #[cfg(windows)]
        {
            let real = temp.join(format!("keel-report-real-{}", std::process::id()));
            let link = temp.join(format!("keel-report-link-{}", std::process::id()));
            std::fs::create_dir_all(&real).unwrap();
            let _ = std::fs::remove_dir(&link);
            keel_vfs::fsattr::make_link(&real, &link, keel_vfs::LinkKind::Junction).unwrap();
            assert!(!report_ok_in(&link.join("keel-syscleanup-1-2.json"), &real));
            let _ = std::fs::remove_dir(&link);
            let _ = std::fs::remove_dir_all(&real);
        }
    }

    #[test]
    fn a_late_preview_never_swaps_the_task_of_the_open_dialog() {
        let info = |task: Task| -> Result<TaskInfo, String> {
            Ok(TaskInfo {
                steps: steps_in(&task, &dirs(true)).unwrap(),
                task,
                size: Some(1),
                note: String::new(),
            })
        };
        let vhd_task = Task::OptimizeVhd {
            path: vhd(),
            powershell: true,
        };
        let old = TaskDialog::new(Task::OptimizeVhd {
            path: vhd(),
            powershell: false,
        });
        let mut d = TaskDialog::new(Task::Hibernation);
        assert_ne!(old.request, d.request);
        // The earlier dialog's answer arrives: dropped.
        assert!(!d.answer(old.request, info(vhd_task.clone())));
        assert!(d.info.is_none());
        // Its own request but another task: dropped.
        assert!(!d.answer(d.request, info(vhd_task.clone())));
        assert!(d.answer(d.request, info(Task::Hibernation)));
        d.press(Press::Continue);
        assert!(d.confirming);
        // Nothing replaces the preview once it is there (or while confirming).
        assert!(!d.answer(d.request, info(Task::Hibernation)));
        assert_eq!(
            d.press(Press::Run).0,
            Some(Action::System(SysCmd::Run(Task::Hibernation)))
        );
        // A VHD dialog takes the method decided for its own disk.
        let mut v = TaskDialog::new(Task::OptimizeVhd {
            path: vhd(),
            powershell: false,
        });
        let other = Task::OptimizeVhd {
            path: vhd().with_file_name("Other.vhdx"),
            powershell: true,
        };
        assert!(!v.answer(v.request, info(other)));
        assert!(v.answer(v.request, info(vhd_task.clone())));
        assert_eq!(v.task, vhd_task);
    }

    #[test]
    fn vhd_paths_are_checked() {
        for bad in ["disk.vhdx", "/tmp/disk.iso", "C:\\x\\disk.txt"] {
            let path = PathBuf::from(bad);
            assert!(check_vhd(&path).is_err(), "{bad}");
            assert!(steps(&Task::OptimizeVhd {
                path,
                powershell: false
            })
            .is_err());
        }
        assert!(check_vhd(&vhd()).is_ok());
        let upper = vhd().with_extension("VHD");
        assert!(check_vhd(&upper).is_ok());
    }

    #[test]
    fn helper_arguments_round_trip() {
        let report = std::env::temp_dir().join("keel-syscleanup-1-2.json");
        for task in [
            Task::Hibernation,
            Task::DismAnalyze,
            Task::DismCleanup,
            Task::OptimizeVhd {
                path: vhd(),
                powershell: true,
            },
            Task::OptimizeVhd {
                path: vhd(),
                powershell: false,
            },
        ] {
            assert_eq!(Task::from_arg(&task.to_arg()), Ok(task.clone()));
            let args = helper_args(&task, &report);
            assert!(args.starts_with(&format!("--system-cleanup \"{}\"", task.to_arg())));
            assert!(args.ends_with(&format!("--cleanup-report \"{}\"", report.display())));
        }
        assert_eq!(Task::Hibernation.to_arg(), "hibernate-off");
        assert!(Task::from_arg("format-c").is_err());
        assert!(Task::from_arg("vhd-ps:relative.vhdx").is_err());
        assert!(Task::from_arg("vhd-ps:C:\\x\\disk.exe").is_err());
        assert!(report_ok(&report));
        assert!(!report_ok(Path::new("report.json")));
        assert!(!report_ok(&std::env::temp_dir().join("other.json")));
    }

    #[test]
    fn helper_refuses_unknown_tasks_without_running_anything() {
        assert_eq!(helper_main("rm -rf", None), 2);
    }

    #[test]
    fn preview_then_second_confirmation_then_run() {
        let mut d = TaskDialog::new(Task::Hibernation);
        // Nothing until the preview is measured.
        assert_eq!(d.press(Press::Continue), (None, false));
        assert_eq!(d.press(Press::Run), (None, false));
        d.info = Some(Ok(TaskInfo {
            task: Task::Hibernation,
            size: Some(6_400_000_000),
            note: String::new(),
            steps: steps(&Task::Hibernation).unwrap(),
        }));
        // Run is not on the preview: Continue first.
        assert_eq!(d.press(Press::Run), (None, false));
        assert_eq!(d.press(Press::Continue), (None, false));
        assert!(d.confirming);
        let text = d.confirm_text();
        assert!(text.contains("powercfg.exe /h off"), "{text}");
        assert!(text.contains("hiberfil.sys (6.40 GB) is deleted"), "{text}");
        assert!(text.contains("as administrator"), "{text}");
        assert_eq!(d.press(Press::Back), (None, false));
        assert!(!d.confirming);
        d.press(Press::Continue);
        assert_eq!(
            d.press(Press::Run),
            (Some(Action::System(SysCmd::Run(Task::Hibernation))), true)
        );
        assert_eq!(d.press(Press::Cancel), (None, true));
        // A failed preview never runs.
        let mut bad = TaskDialog::new(Task::DismCleanup);
        bad.info = Some(Err("no".into()));
        bad.press(Press::Continue);
        assert_eq!(bad.press(Press::Run), (None, false));
    }

    #[test]
    fn the_previewed_vhd_method_is_what_runs() {
        let mut d = TaskDialog::new(Task::OptimizeVhd {
            path: vhd(),
            powershell: false,
        });
        let decided = Task::OptimizeVhd {
            path: vhd(),
            powershell: true,
        };
        d.info = Some(Ok(TaskInfo {
            task: decided.clone(),
            size: Some(1),
            note: String::new(),
            steps: steps_in(&decided, &dirs(true)).unwrap(),
        }));
        d.press(Press::Continue);
        assert_eq!(
            d.press(Press::Run).0,
            Some(Action::System(SysCmd::Run(decided)))
        );
    }

    const ANALYSIS: &str = "\
Deployment Image Servicing and Management tool
Version: 10.0.26100.1

Image Version: 10.0.26100.1

[===========================100.0%==========================]

Component Store (WinSxS) information:

Windows Explorer Reported Size of Component Store : 9.12 GB

Actual Size of Component Store : 8.25 GB

    Shared with Windows : 5.52 GB
    Backups and Disabled Features : 2.38 GB
    Cache and Temporary Data :  346.43 MB

Date of Last Cleanup : 2026-09-01 10:11:12

Number of Reclaimable Packages : 3
Component Store Cleanup Recommended : Yes

The operation completed successfully.
";

    #[test]
    fn parses_the_component_store_analysis() {
        let r = parse_analysis(ANALYSIS);
        let gib = |x: f64| (x * 1024.0 * 1024.0 * 1024.0).round() as u64;
        assert_eq!(r.actual, Some(gib(8.25)));
        assert_eq!(r.backups, Some(gib(2.38)));
        assert_eq!(r.cache, Some((346.43f64 * 1024.0 * 1024.0).round() as u64));
        assert_eq!(r.packages, Some(3));
        assert_eq!(r.recommended, Some(true));
        assert_eq!(r.reclaimable(), Some(r.backups.unwrap() + r.cache.unwrap()));
        let s = r.summary();
        assert!(
            s.contains("3 reclaimable packages") && s.ends_with("cleanup recommended"),
            "{s}"
        );
        assert_eq!(parse_analysis("garbage\n : \n"), StoreReport::default());
        assert_eq!(StoreReport::default().reclaimable(), None);
        assert_eq!(parse_size("12 bytes"), Some(12));
        assert_eq!(parse_size("1,024.5 KB"), Some(1_049_088));
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("1 XB"), None);
    }

    #[test]
    fn launchers_per_system() {
        use Launch::*;
        let w = |l| launch_for(Os::Windows, l, None);
        assert_eq!(
            w(Launcher::DiskCleanup),
            Some(Program("cleanmgr.exe", Vec::new()))
        );
        assert_eq!(
            launch_for(Os::Windows, Launcher::DiskCleanup, Some("d:\\")),
            Some(Program("cleanmgr.exe", vec!["/d".into(), "D".into()]))
        );
        // Not a drive letter: Disk Cleanup asks for the drive itself.
        assert_eq!(
            launch_for(Os::Windows, Launcher::DiskCleanup, Some("/media/usb")),
            Some(Program("cleanmgr.exe", Vec::new()))
        );
        assert_eq!(
            w(Launcher::InstalledApps),
            Some(Url("ms-settings:appsfeatures"))
        );
        assert_eq!(
            w(Launcher::ProgramsAndFeatures),
            Some(Program("control.exe", vec!["appwiz.cpl".into()]))
        );
        assert_eq!(
            w(Launcher::StorageSettings),
            Some(Url("ms-settings:storagesense"))
        );
        assert_eq!(
            launch_for(Os::Mac, Launcher::StorageSettings, None),
            Some(Url("x-apple.systempreferences:com.apple.settings.Storage"))
        );
        assert_eq!(launch_for(Os::Mac, Launcher::DiskCleanup, None), None);
        assert!(launchers(Os::Linux).is_empty());
        for l in launchers(Os::Windows).iter().chain(launchers(Os::Mac)) {
            let os = if launchers(Os::Mac).contains(l) {
                Os::Mac
            } else {
                Os::Windows
            };
            assert!(launch_for(os, *l, None).is_some(), "{l:?}");
        }
    }

    #[test]
    fn palette_has_no_shortcuts_and_delete_needs_a_selection() {
        let with = palette_items(true);
        let without = palette_items(false);
        assert!(with.iter().all(|i| i.shortcut.is_empty()));
        assert!(with.iter().any(|i| i.label == "Delete permanently…"));
        assert!(!without.iter().any(|i| i.label == "Delete permanently…"));
        if OS == Os::Windows {
            assert!(without.iter().any(|i| i.label == "Disable hibernation…"));
            assert!(without.iter().any(|i| i.label == "Optimize VHD/VHDX…"));
        }
    }

    #[test]
    fn delete_permanently_measures_asks_for_the_phrase_then_deletes() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let dir = std::env::temp_dir().join(format!("keel-permdel-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("b.txt"), b"stays").unwrap();
        let mut state = AppState::new(
            egui::Context::default(),
            Arc::new(keel_vfs::Router::new()),
            VPath::local(&dir),
        );
        let settle = |state: &mut AppState, done: &dyn Fn(&AppState) -> bool| {
            let until = Instant::now() + Duration::from_secs(10);
            while !done(state) && Instant::now() < until {
                if let Ok(msg) = state.rx.recv_timeout(Duration::from_millis(50)) {
                    state.apply(msg);
                }
            }
            assert!(done(state), "timed out waiting for workers");
        };
        settle(&mut state, &|s| !s.tab(0).loading);
        state.tab_mut(0).cursor = Some("a.txt".into());
        state.run(0, Action::System(SysCmd::DeletePermanently));
        settle(&mut state, &|s| s.dialog.is_some());
        let Some(Dialog::System(d)) = state.dialog.take() else {
            panic!("the typed confirmation");
        };
        let SysDialog::DeletePermanently(mut c) = *d else {
            panic!("the typed confirmation");
        };
        assert_eq!(c.phrase(), "delete 1");
        assert!(c.text().contains("1 item (1 file, 5 B)"), "{}", c.text());
        assert!(
            dir.join("a.txt").exists(),
            "nothing deleted before the answer"
        );
        c.typed = "delete 2".into();
        assert_eq!(c.answer(), None);
        c.typed = "delete 1".into();
        let yes = c.answer().expect("the phrase");
        state.run(0, yes);
        settle(&mut state, &|_| !dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());
        // The undo journal says it cannot be undone.
        assert_eq!(state.undo.label().as_deref(), Some("Undo"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
