//! Bandizip's "새 폴더" on a folder's background menu (shell_CreateNewFolder):
//! make "새 폴더" (or "새 폴더 (2)", …) and put it into rename mode in the
//! Explorer window it was asked from, as Windows' own 새로 만들기 → 폴더 does.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect()
}

/// Creates the first free "새 폴더" name in `dir`; creating is the test, so two
/// clicks at once cannot get the same folder.
pub fn create(dir: &Path) -> Result<PathBuf> {
    if !dir.is_dir() {
        bail!("여기에는 폴더를 만들 수 없습니다: {}", dir.display());
    }
    for n in 1..10_000 {
        let p = if n == 1 { dir.join("새 폴더") } else { dir.join(format!("새 폴더 ({n})")) };
        match std::fs::create_dir(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("{}", p.display())),
        }
    }
    bail!("새 폴더 이름이 모두 쓰이고 있습니다")
}

/// Shows the new folder selected with its name open for editing. Explorer reuses
/// the window already showing `dir` (the one that was right-clicked).
pub fn select_for_rename(dir: &Path, folder: &Path) {
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHChangeNotify, SHOpenFolderAndSelectItems, OFASI_EDIT, SHCNE_MKDIR, SHCNF_FLUSH, SHCNF_PATHW};
    let (d, f) = (wide(dir), wide(folder));
    unsafe {
        let com = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) >= 0;
        // The view has to know the folder before it can select it.
        SHChangeNotify(SHCNE_MKDIR as i32, SHCNF_PATHW | SHCNF_FLUSH, f.as_ptr() as *const _, std::ptr::null());
        let (parent, child) = (ILCreateFromPathW(d.as_ptr()), ILCreateFromPathW(f.as_ptr()));
        // We were started by the click in Explorer, so we may hand the foreground back to it:
        // the name box only opens in a window that is in front.
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY);
        if !parent.is_null() && !child.is_null() {
            let items = [child as *const _];
            SHOpenFolderAndSelectItems(parent, 1, items.as_ptr(), OFASI_EDIT);
        }
        ILFree(parent);
        ILFree(child);
        if com {
            CoUninitialize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_count_up_and_never_reuse() {
        let dir = std::env::temp_dir().join(format!("dz-newfolder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let names: Vec<String> = (0..3).map(|_| create(&dir).unwrap().file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["새 폴더", "새 폴더 (2)", "새 폴더 (3)"]);
        std::fs::remove_dir(dir.join("새 폴더 (2)")).unwrap();
        assert_eq!(create(&dir).unwrap().file_name().unwrap(), "새 폴더 (2)");
        assert!(create(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
