//! Explorer integration, all under HKCU (no admin rights, nothing outside this user).
//!
//! Every menu item is a static verb served by `dz_shell.dll` (IExplorerCommand,
//! so it gets the whole selection at once and decides its own title and
//! visibility). The settings pick the items and the layout, as in Bandizip:
//!
//! ```text
//! flat:     <base>\shell\dalzip.NN_<id>              (items straight in the menu)
//! cascade:  <base>\shell\dalzip\shell\NN_<id>        (under a "dalzip" submenu)
//! ```
//!
//! `NN` is the item's place in `settings::MENU`, so Explorer's alphabetical
//! verb order matches the settings page. The DLL reads `<id>` back from the
//! key name it is initialised with.

use crate::settings::{Group, Settings, MENU};
use anyhow::{bail, Result};
use std::path::Path;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

pub const CLSID: &str = "{2BEFAA5D-6E6D-44ED-BF55-CE6F77683D4B}";
const PROGID: &str = "dalzip.archive";
const PROGID_ZIP: &str = "dalzip.zip";
const PROGID_ISO: &str = "dalzip.iso";
const BASES: [&str; 3] = ["*", "Directory", "Directory\\Background"];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Key(HKEY);

impl Key {
    fn create(path: &str) -> Result<Key> {
        let mut h: HKEY = std::ptr::null_mut();
        let p = wide(&format!("Software\\Classes\\{path}"));
        let rc = unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut h, std::ptr::null_mut())
        };
        if rc != 0 {
            bail!("RegCreateKeyEx {path}: error {rc}");
        }
        Ok(Key(h))
    }

    /// `name == ""` sets the default value.
    fn set(&self, name: &str, value: &str) -> Result<&Key> {
        let n = wide(name);
        let v = wide(value);
        let rc = unsafe { RegSetValueExW(self.0, if name.is_empty() { std::ptr::null() } else { n.as_ptr() }, 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32) };
        if rc != 0 {
            bail!("RegSetValueEx {name}: error {rc}");
        }
        Ok(self)
    }
}

impl Key {
    /// An empty REG_NONE value, the convention for OpenWithProgids entries.
    fn set_empty(&self, name: &str) -> Result<&Key> {
        let n = wide(name);
        let rc = unsafe { RegSetValueExW(self.0, n.as_ptr(), 0, REG_NONE, std::ptr::null(), 0) };
        if rc != 0 {
            bail!("RegSetValueEx {name}: error {rc}");
        }
        Ok(self)
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn delete_tree(path: &str) {
    let p = wide(&format!("Software\\Classes\\{path}"));
    unsafe {
        RegDeleteTreeW(HKEY_CURRENT_USER, p.as_ptr());
        RegDeleteKeyW(HKEY_CURRENT_USER, p.as_ptr());
    }
}

/// Subkey names of HKCU\Software\Classes\<path>.
fn subkeys(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let p = wide(&format!("Software\\Classes\\{path}"));
    let mut h: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, KEY_READ, &mut h) } != 0 {
        return out;
    }
    for i in 0.. {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        let rc = unsafe { RegEnumKeyExW(h, i, buf.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if rc != 0 {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    unsafe { RegCloseKey(h) };
    out
}

/// Removes every dalzip verb (either layout) so a new layout starts clean.
fn clear_verbs() {
    for base in BASES {
        for k in subkeys(&format!("{base}\\shell")) {
            if k == "dalzip" || k.starts_with("dalzip.") {
                delete_tree(&format!("{base}\\shell\\{k}"));
            }
        }
    }
}

fn verb(path: &str, title: &str, icon: Option<&str>) -> Result<()> {
    // The state handler (same class) hides items that do not fit the selection;
    // CommandStateSync asks for it while the menu is built rather than afterwards.
    let k = Key::create(path)?;
    k.set("MUIVerb", title)?
        .set("ExplorerCommandHandler", CLSID)?
        .set("CommandStateHandler", CLSID)?
        .set("CommandStateSync", "")?
        .set("MultiSelectModel", "Player")?;
    if let Some(i) = icon {
        k.set("Icon", i)?;
    }
    Ok(())
}

/// Which bases an item group belongs on.
fn bases_for(g: Group) -> &'static [&'static str] {
    match g {
        Group::Archives => &["*"],
        Group::Sources => &["*", "Directory"],
        Group::Folder => &["Directory", "Directory\\Background"],
    }
}

pub fn register(dir: &Path) -> Result<String> {
    let gui = dir.join("dzui.exe");
    let dll = dir.join("dz_shell.dll");
    for f in [&gui, &dll] {
        if !f.exists() {
            bail!("{} is missing", f.display());
        }
    }
    let s = Settings::load();
    let gui = gui.display().to_string();
    let icon = format!("{gui},0");

    Key::create(&format!("CLSID\\{CLSID}"))?.set("", "dalzip shell command")?;
    Key::create(&format!("CLSID\\{CLSID}\\InprocServer32"))?.set("", &dll.display().to_string())?.set("ThreadingModel", "Apartment")?;

    clear_verbs();
    let mut count = 0;
    for base in BASES {
        let items: Vec<(usize, &str, &str)> = MENU
            .iter()
            .enumerate()
            .filter(|(i, (_, _, g, _))| s.enabled[*i] && bases_for(*g).contains(&base))
            .map(|(i, (id, label, _, _))| (i, *id, *label))
            .collect();
        if items.is_empty() {
            continue;
        }
        // The console item is a plain command, as in Bandizip (`cmd.exe /s /k pushd "%s"`):
        // a background click hands verbs no selection, so it cannot go through the DLL.
        let (cmd, items): (Vec<_>, Vec<_>) = items.into_iter().partition(|(_, id, _)| *id == "f_cmd");
        for (i, id, _) in &cmd {
            let path = format!("{base}\\shell\\dalzip.{i:02}_{id}");
            Key::create(&path)?.set("MUIVerb", "명령 창 열기(&C)")?.set("Icon", "cmd.exe")?;
            Key::create(&format!("{path}\\command"))?.set("", "cmd.exe /s /k pushd \"%V\"")?;
        }
        if s.cascade && !items.is_empty() {
            let root = format!("{base}\\shell\\dalzip");
            Key::create(&root)?.set("MUIVerb", "dalzip")?.set("Icon", &icon)?.set("SubCommands", "")?;
            for (i, id, label) in &items {
                verb(&format!("{root}\\shell\\{i:02}_{id}"), label, None)?;
            }
        } else {
            for (i, id, label) in &items {
                verb(&format!("{base}\\shell\\dalzip.{i:02}_{id}"), label, Some(&icon))?;
            }
        }
        count += items.len() + cmd.len();
    }

    let open = match s.double_click {
        1 => format!("\"{gui}\" x --target auto \"%1\""),
        2 => format!("\"{gui}\" x --target name \"%1\""),
        _ => format!("\"{gui}\" \"%1\""),
    };
    // Icon resources 101 (.dz), 102 (.zip) and 116 (.iso) come from build.rs.
    for (progid, name, icon_id) in [(PROGID, "dalzip 압축 파일", 101), (PROGID_ZIP, "압축(ZIP) 파일", 102), (PROGID_ISO, "디스크 이미지 파일", 116)] {
        Key::create(progid)?.set("", name)?;
        Key::create(&format!("{progid}\\DefaultIcon"))?.set("", &format!("{gui},-{icon_id}"))?;
        Key::create(&format!("{progid}\\shell\\open"))?.set("FriendlyAppName", "dalzip")?;
        Key::create(&format!("{progid}\\shell\\open\\command"))?.set("", &open)?;
    }
    Key::create(".dz")?.set("", PROGID)?.set("PerceivedType", "compressed")?;
    // .zip belongs to whatever the user picked (a hash-protected UserChoice, Bandizip here);
    // dalzip only offers itself in "Open with", where the user can make it the default.
    Key::create(".zip\\OpenWithProgids")?.set_empty(PROGID_ZIP)?;
    // The same for .iso, which Windows itself opens by mounting.
    Key::create(".iso\\OpenWithProgids")?.set_empty(PROGID_ISO)?;

    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    Ok(format!(
        "registered: {count} menu entries ({}), .dz -> {gui}, offered for .zip and .iso",
        if s.cascade { "cascade" } else { "flat" }
    ))
}

pub fn unregister() -> Result<String> {
    clear_verbs();
    delete_tree(&format!("CLSID\\{CLSID}"));
    delete_tree(PROGID);
    delete_tree(PROGID_ZIP);
    delete_tree(PROGID_ISO);
    delete_tree(".dz");
    for (ext, progid) in [(".zip", PROGID_ZIP), (".iso", PROGID_ISO)] {
        let (k, v) = (wide(&format!("Software\\Classes\\{ext}\\OpenWithProgids")), wide(progid));
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr()) };
    }
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
    Ok("탐색기 메뉴와 .dz 연결을 제거했습니다".into())
}
