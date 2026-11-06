//! dz_shell.dll — dalzip's Explorer menu.
//!
//! Bandizip ships two shell DLLs: an IContextMenu one for the classic menu and
//! an IExplorerCommand one that its signed sparse MSIX package exposes on the
//! Windows 11 menu. This DLL is the IExplorerCommand kind, hooked into the
//! classic menu through static verbs (`ExplorerCommandHandler`), which needs
//! no package or signing. `dzui --register` writes the verbs flat (Bandizip's
//! default) or as a cascade, per the user's settings:
//!
//! ```text
//! flat:    HKCU\Software\Classes\<base>\shell\dalzip.NN_<id>          ExplorerCommandHandler={CLSID}
//! cascade: HKCU\Software\Classes\<base>\shell\dalzip\shell\NN_<id>    (under a "dalzip" submenu)
//! ```
//!
//! Explorer hands each verb's key name to `IInitializeCommand::Initialize`, so
//! one class serves every item. The selection arrives whole (an
//! IShellItemArray), is written to a list file in %TEMP% and handed to
//! `dzui.exe <command> --list <file>` — Bandizip passes it through a named
//! shared-memory section (`/usemmf bzfmmf…`) for the same reason: command-line
//! length.
//!
//! Everything here runs inside explorer.exe, so every entry point catches
//! panics and turns them into E_FAIL rather than taking Explorer down.

#![allow(non_snake_case)]
// MSVC asks for COM exports to be PRIVATE (LNK4104); harmless for a shell extension.
#![allow(linker_messages)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::UI::Shell::*;
use windows_core::*;

/// Must match `CLSID` in src/bin/dzui/register.rs.
pub const CLSID_DALZIP: GUID = GUID::from_u128(0x2BEFAA5D_6E6D_44ED_BF55_CE6F77683D4B);

const ECS_ENABLED: u32 = 0;
const ECS_HIDDEN: u32 = 2;

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

#[derive(Clone, Copy, PartialEq)]
enum Needs {
    /// Anything that is not only archives (.dz / .zip / .iso).
    Sources,
    /// Two or more non-archive items.
    SourcesMany,
    /// Only archives (.dz / .zip / .iso).
    Archives,
    OneArchive,
}

/// (item id, dzui arguments, what the selection must be). Ids match
/// `MENU` in src/bin/dzui/settings.rs; registry keys are `[dalzip.]NN_<id>`.
/// (`f_cmd` and `f_newfolder` are plain registry commands, not handled here.)
const VERBS: &[(&str, &str, Needs)] = &[
    ("x_here", "x --target here", Needs::Archives),
    ("x_auto", "x --target auto", Needs::Archives),
    ("x_name", "x --target name", Needs::Archives),
    ("x_here_del", "x --target here --delete", Needs::Archives),
    ("x_auto_del", "x --target auto --delete", Needs::Archives),
    ("x_name_del", "x --target name --delete", Needs::Archives),
    ("x_dlg", "x --target dlg", Needs::Archives),
    ("x_test", "t", Needs::Archives),
    ("x_open", "open", Needs::OneArchive),
    ("c_dz", "cq", Needs::Sources),
    ("c_zip", "cq --format zip", Needs::Sources),
    ("c_dz_del", "cq --delete", Needs::Sources),
    ("c_each", "ce", Needs::SourcesMany),
    ("c_each_zip", "ce --format zip", Needs::SourcesMany),
    ("c_rule", "cq --rule", Needs::Sources),
    ("c_dlg", "c", Needs::Sources),
];

/// `[dalzip.]NN_<id>` -> `<id>`.
fn verb_id(key: &str) -> &str {
    let k = key.strip_prefix("dalzip.").unwrap_or(key);
    k.split_once('_').map_or(k, |(_, id)| id)
}

/// The date rule from dzui's settings, expanded for "now" (local time).
fn rule_name(stem: &str) -> String {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let rule = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, w!("Software\\dalzip"), w!("name_rule"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut c_void), Some(&mut size))
    };
    let rule = if rule.is_ok() && size > 2 {
        String::from_utf16_lossy(&buf[..size as usize / 2 - 1])
    } else {
        "${NAME} %Y-%m-%d %H-%M-%S".to_string()
    };
    let t = unsafe { GetLocalTime() };
    rule.replace("${NAME}", stem)
        .replace("%Y", &format!("{:04}", t.wYear))
        .replace("%m", &format!("{:02}", t.wMonth))
        .replace("%d", &format!("{:02}", t.wDay))
        .replace("%H", &format!("{:02}", t.wHour))
        .replace("%M", &format!("{:02}", t.wMinute))
        .replace("%S", &format!("{:02}", t.wSecond))
}

/// Bandizip's "${name}" for compress items: one item -> its name, several -> their folder's.
fn compress_name(sel: &[String]) -> String {
    match sel {
        [one] => file_name(one, true),
        [first, ..] => Path::new(first).parent().map(|d| file_name(&d.to_string_lossy(), false)).unwrap_or_default(),
        [] => String::new(),
    }
}


fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| Err(E_FAIL.into()))
}

fn dup(s: &str) -> Result<PWSTR> {
    let h = HSTRING::from(s);
    unsafe { SHStrDupW(&h) }
}

/// Directory holding this DLL (and dzui.exe next to it).
fn install_dir() -> Option<PathBuf> {
    let module = HMODULE(MODULE.load(Ordering::Relaxed));
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n])).parent().map(Path::to_path_buf)
}

/// File-system paths of the selection; virtual items (Control Panel, phones, …) yield an error.
fn selection(items: Ref<IShellItemArray>) -> Result<Vec<String>> {
    let items = items.ok()?;
    let mut out = Vec::new();
    unsafe {
        for i in 0..items.GetCount()? {
            let item = items.GetItemAt(i)?;
            let p = item.GetDisplayName(SIGDN_FILESYSPATH)?;
            let s = p.to_string();
            CoTaskMemFree(Some(p.0 as *const c_void));
            out.push(s.map_err(|_| Error::from(E_FAIL))?);
        }
    }
    Ok(out)
}

/// Archives dzui can open: .dz, .zip and .iso.
fn is_archive(p: &str) -> bool {
    Path::new(p).extension().is_some_and(|e| ["dz", "zip", "iso"].iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn file_name(p: &str, stem: bool) -> String {
    let path = Path::new(p);
    let n = if stem && !path.is_dir() { path.file_stem() } else { path.file_name() };
    n.map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Registered as each verb's command and also its `CommandStateHandler`
/// (with `CommandStateSync`), so the shell asks for visibility while it builds
/// the menu. Without those two values every item showed regardless of state.
#[implement(IExplorerCommand, IExplorerCommandState, IInitializeCommand)]
#[derive(Default)]
struct Command {
    verb: RefCell<String>,
}

impl Command_Impl {
    fn entry(&self) -> Option<&'static (&'static str, &'static str, Needs)> {
        let v = self.verb.borrow();
        let id = verb_id(v.as_str());
        VERBS.iter().find(|(k, _, _)| *k == id)
    }

    fn state(&self, items: Ref<IShellItemArray>) -> Result<u32> {
        guard(|| {
            let Some((_, _, needs)) = self.entry() else { return Ok(ECS_HIDDEN) };
            let Ok(sel) = selection(items) else { return Ok(ECS_HIDDEN) };
            let archives = !sel.is_empty() && sel.iter().all(|p| is_archive(p));
            let show = !sel.is_empty()
                && match needs {
                    Needs::Sources => !archives,
                    Needs::SourcesMany => !archives && sel.len() > 1,
                    Needs::Archives => archives,
                    Needs::OneArchive => archives && sel.len() == 1,
                };
            Ok(if show { ECS_ENABLED } else { ECS_HIDDEN })
        })
    }
}

impl IExplorerCommandState_Impl for Command_Impl {
    fn GetState(&self, items: Ref<IShellItemArray>, _ok_to_be_slow: BOOL) -> Result<u32> {
        self.state(items)
    }
}

impl IInitializeCommand_Impl for Command_Impl {
    fn Initialize(&self, name: &PCWSTR, _bag: Ref<IPropertyBag>) -> Result<()> {
        guard(|| {
            let s = unsafe { name.to_string() }.map_err(|_| Error::from(E_INVALIDARG))?;
            *self.verb.borrow_mut() = s;
            Ok(())
        })
    }
}

impl IExplorerCommand_Impl for Command_Impl {
    fn GetTitle(&self, items: Ref<IShellItemArray>) -> Result<PWSTR> {
        guard(|| {
            let (id, _, _) = self.entry().ok_or_else(|| Error::from(E_FAIL))?;
            let sel = selection(items).unwrap_or_default();
            let many = sel.len() > 1;
            let one_stem = || sel.first().map(|p| file_name(p, true)).unwrap_or_default();
            let name = compress_name(&sel);
            // Wording and access keys follow Bandizip's menu (langs\Korean.lang [SHELL]).
            let title = match *id {
                "x_here" => "여기에 풀기(&X)".into(),
                "x_auto" if many => "각각 알아서 풀기(&1)".into(),
                "x_auto" => "알아서 풀기(&Z)".into(),
                "x_name" if many => "각각 파일명 폴더에 풀기(&E)".into(),
                "x_name" => format!("{}\\ 에 풀기(&E)", one_stem()),
                "x_here_del" => "여기에 풀고 삭제하기".into(),
                "x_auto_del" if many => "각각 알아서 풀고 삭제하기".into(),
                "x_auto_del" => "알아서 풀고 삭제하기".into(),
                "x_name_del" if many => "각각 파일명 폴더에 풀고 삭제하기".into(),
                "x_name_del" => format!("{}\\ 에 풀고 삭제하기", one_stem()),
                "x_dlg" => "dalzip으로 압축 풀기(&B)...".into(),
                "x_test" => "압축 파일 테스트(&T)".into(),
                "x_open" => "dalzip으로 열기".into(),
                "c_dz" => format!("{name}.dz 로 압축하기(&Q)"),
                "c_zip" => format!("{name}.zip 으로 압축하기(&Z)"),
                "c_dz_del" => format!("{name}.dz 로 압축하고 원본 삭제"),
                "c_each" => "각각 파일명.dz로 압축하기(&U)".into(),
                "c_each_zip" => "각각 파일명.zip으로 압축하기".into(),
                "c_rule" => format!("{}.dz 로 압축하기", rule_name(&name)),
                "c_dlg" => "dalzip으로 압축하기(&L)...".into(),
                _ => return Err(E_FAIL.into()),
            };
            dup(&title)
        })
    }

    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        guard(|| {
            let dir = install_dir().ok_or_else(|| Error::from(E_FAIL))?;
            dup(&format!("{},0", dir.join("dzui.exe").display()))
        })
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Err(E_NOTIMPL.into())
    }

    fn GetState(&self, items: Ref<IShellItemArray>, _ok_to_be_slow: BOOL) -> Result<u32> {
        self.state(items)
    }

    fn Invoke(&self, items: Ref<IShellItemArray>, _bind: Ref<IBindCtx>) -> Result<()> {
        guard(|| {
            let (_, args, _) = self.entry().ok_or_else(|| Error::from(E_FAIL))?;
            let sel = selection(items)?;
            let gui = install_dir().ok_or_else(|| Error::from(E_FAIL))?.join("dzui.exe");
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let list = std::env::temp_dir().join(format!("dalzip-{}-{stamp}.lst", std::process::id()));
            std::fs::write(&list, sel.join("\n")).map_err(|_| Error::from(E_FAIL))?;
            let mut cmd = std::process::Command::new(gui);
            cmd.args(args.split(' ')).arg("--list").arg(&list);
            if let Some(dir) = sel.first().and_then(|p| Path::new(p).parent()) {
                cmd.current_dir(dir);
            }
            cmd.spawn().map_err(|_| Error::from(E_FAIL))?;
            Ok(())
        })
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(0) // ECF_DEFAULT; the cascade itself comes from the registry
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        guard(|| {
            if !outer.is_null() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let cmd: IUnknown = Command::default().into();
            unsafe { cmd.query(riid, ppv).ok() }
        })
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

#[no_mangle]
extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module.0, Ordering::Relaxed);
    }
    TRUE
}

#[no_mangle]
unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() || clsid.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if *clsid != CLSID_DALZIP {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    std::panic::catch_unwind(|| {
        let f: IClassFactory = Factory.into();
        f.query(riid, ppv)
    })
    .unwrap_or(E_FAIL)
}

/// Stay loaded: Explorer keeps menu handlers around anyway, and unloading a
/// Rust DLL with live COM objects is not worth the bookkeeping.
#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
