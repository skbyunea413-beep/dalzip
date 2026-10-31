//! User settings in HKCU\Software\dalzip, after Bandizip's shell settings page
//! (IDD_SETTING_SHELL): which Explorer items to show, flat or cascaded,
//! what double-clicking a .dz does, and the dated file-name rule.

use windows_sys::Win32::System::Registry::*;

const KEY: &str = "Software\\dalzip";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Shown when every selected item is a .dz archive.
    Archives,
    /// Shown for files and folders that are not (only) archives.
    Sources,
    /// Shown on a folder or a folder's background.
    Folder,
}

/// Explorer menu items: (id, label in settings, group, on by default).
/// The ids are what `shell/src/lib.rs` maps to commands; keep them in sync.
pub const MENU: &[(&str, &str, Group, bool)] = &[
    ("x_here", "여기에 풀기", Group::Archives, true),
    ("x_auto", "알아서 풀기", Group::Archives, true),
    ("x_name", "파일명 폴더에 풀기", Group::Archives, true),
    ("x_here_del", "여기에 풀고 삭제하기", Group::Archives, false),
    ("x_auto_del", "알아서 풀고 삭제하기", Group::Archives, false),
    ("x_name_del", "파일명 폴더에 풀고 삭제하기", Group::Archives, false),
    ("x_dlg", "dalzip으로 압축 풀기...", Group::Archives, true),
    ("x_test", "압축 파일 테스트", Group::Archives, false),
    ("x_open", "dalzip으로 열기", Group::Archives, true),
    ("c_dz", "\"파일명.dz\"로 압축하기", Group::Sources, true),
    ("c_zip", "\"파일명.zip\"으로 압축하기 (빠른 zip)", Group::Sources, true),
    ("c_dz_del", "\"파일명.dz\"로 압축하고 원본 삭제 (테스트 통과 후 휴지통)", Group::Sources, false),
    ("c_each", "각각의 \"파일명.dz\"로 압축하기", Group::Sources, true),
    ("c_each_zip", "각각의 \"파일명.zip\"으로 압축하기", Group::Sources, false),
    ("c_rule", "파일명에 규칙을 넣어 압축하기", Group::Sources, false),
    ("c_dlg", "dalzip으로 압축하기...", Group::Sources, true),
    ("f_cmd", "명령 창(cmd) 열기", Group::Folder, false),
];

pub const DEFAULT_RULE: &str = "${NAME} %Y-%m-%d %H-%M-%S";

#[derive(Clone)]
pub struct Settings {
    pub enabled: Vec<bool>,
    /// Bandizip's "탐색기 메뉴를 계단식으로 보여주기"; off = items directly in the menu.
    pub cascade: bool,
    /// 0 open, 1 smart-extract next to it, 2 extract into <name>\.
    pub double_click: u32,
    pub rule: String,
    /// After extracting from the dalzip window: close it (default on).
    pub extract_close: bool,
    /// After extracting from the dalzip window: open the output folder (default on, as Bandizip).
    pub extract_open: bool,
    /// Extract dialog's last choices, kept when `extract_remember` is on (Bandizip's "설정 기억").
    pub extract_target: u32,
    pub extract_delete: bool,
    pub extract_remember: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn get_dword(name: &str) -> Option<u32> {
    let (k, n) = (wide(KEY), wide(name));
    let mut v: u32 = 0;
    let mut size = 4u32;
    let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut v as *mut u32 as *mut _, &mut size) };
    (rc == 0).then_some(v)
}

fn get_string(name: &str) -> Option<String> {
    let (k, n) = (wide(KEY), wide(name));
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut size) };
    if rc != 0 {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..len]))
}

impl Settings {
    pub fn load() -> Settings {
        Settings {
            enabled: MENU.iter().map(|(id, _, _, def)| get_dword(&format!("menu_{id}")).map_or(*def, |v| v != 0)).collect(),
            cascade: get_dword("menu_cascade").is_some_and(|v| v != 0),
            double_click: get_dword("double_click").unwrap_or(0).min(2),
            rule: get_string("name_rule").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| DEFAULT_RULE.into()),
            extract_close: get_dword("extract_close").map_or(true, |v| v != 0),
            extract_open: get_dword("extract_open").map_or(true, |v| v != 0),
            extract_target: get_dword("extract_target").unwrap_or(0).min(2),
            extract_delete: get_dword("extract_delete").is_some_and(|v| v != 0),
            extract_remember: get_dword("extract_remember").map_or(true, |v| v != 0),
        }
    }

    /// What to do after an extraction started from the dalzip window.
    pub fn after_extract(&self) -> crate::jobs::After {
        crate::jobs::After { close: self.extract_close, open_folder: self.extract_open }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let mut h: HKEY = std::ptr::null_mut();
        let k = wide(KEY);
        let rc = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, k.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut h, std::ptr::null_mut()) };
        if rc != 0 {
            anyhow::bail!("cannot open {KEY}: error {rc}");
        }
        let set_dword = |name: &str, v: u32| {
            let n = wide(name);
            unsafe { RegSetValueExW(h, n.as_ptr(), 0, REG_DWORD, &v as *const u32 as *const u8, 4) };
        };
        for ((id, ..), on) in MENU.iter().zip(&self.enabled) {
            set_dword(&format!("menu_{id}"), *on as u32);
        }
        set_dword("menu_cascade", self.cascade as u32);
        set_dword("double_click", self.double_click);
        set_dword("extract_close", self.extract_close as u32);
        set_dword("extract_open", self.extract_open as u32);
        set_dword("extract_target", self.extract_target);
        set_dword("extract_delete", self.extract_delete as u32);
        set_dword("extract_remember", self.extract_remember as u32);
        let n = wide("name_rule");
        let v = wide(&self.rule);
        unsafe {
            RegSetValueExW(h, n.as_ptr(), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32);
            RegCloseKey(h);
        }
        Ok(())
    }
}

/// Expands `${NAME}` and strftime-style `%Y %m %d %H %M %S` (local time).
pub fn expand_rule(rule: &str, name: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, mo, d, h, mi, s) = dalzip::util::civil(now + dalzip::util::local_offset_secs());
    rule.replace("${NAME}", name)
        .replace("%Y", &format!("{y:04}"))
        .replace("%m", &format!("{mo:02}"))
        .replace("%d", &format!("{d:02}"))
        .replace("%H", &format!("{h:02}"))
        .replace("%M", &format!("{mi:02}"))
        .replace("%S", &format!("{s:02}"))
}
