//! Builds the shell's own context menu (the one Explorer shows under "Show
//! more options") and prints it, submenus included, so the dalzip items can be
//! checked without clicking.
//!
//! usage: menu_dump <path>...        menu for that selection (one or many items)
//!        menu_dump --bg <folder>    menu for a click on the folder's background

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_core::*;

const WM_INITMENUPOPUP: u32 = 0x0117;

unsafe fn dump(menu: HMENU, cm2: Option<&IContextMenu2>, depth: usize) {
    let n = GetMenuItemCount(Some(menu));
    for i in 0..n {
        let mut buf = [0u16; 256];
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_SUBMENU | MIIM_FTYPE,
            dwTypeData: PWSTR(buf.as_mut_ptr()),
            cch: buf.len() as u32,
            ..Default::default()
        };
        if GetMenuItemInfoW(menu, i as u32, true, &mut info).is_err() || info.fType.contains(MFT_SEPARATOR) {
            continue;
        }
        let text = String::from_utf16_lossy(&buf[..info.cch as usize]);
        println!("{}{}", "  ".repeat(depth), text);
        if !info.hSubMenu.is_invalid() {
            // Cascades may be filled on demand, as when Explorer opens them.
            if let Some(cm) = cm2 {
                let _ = cm.HandleMenuMsg(WM_INITMENUPOPUP, WPARAM(info.hSubMenu.0 as usize), LPARAM(i as isize));
            }
            dump(info.hSubMenu, cm2, depth + 1);
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let cm: IContextMenu = if args.first().map(String::as_str) == Some("--bg") {
            // The background menu comes from the folder's view object.
            let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(args[1].as_str()), None)?;
            let folder: IShellFolder = item.BindToHandler(None, &BHID_SFObject)?;
            folder.CreateViewObject(windows::Win32::Foundation::HWND::default())?
        } else {
            let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
            for p in &args {
                let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
                SHParseDisplayName(&HSTRING::from(p.as_str()), None, &mut pidl, 0, None)?;
                pidls.push(pidl);
            }
            let items = SHCreateShellItemArrayFromIDLists(&pidls)?;
            items.BindToHandler(None, &BHID_SFUIObject)?
        };
        let menu = CreatePopupMenu()?;
        cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL).ok()?;
        let cm2: Option<IContextMenu2> = cm.cast().ok();
        dump(menu, cm2.as_ref(), 0);
        let _ = DestroyMenu(menu);
    }
    Ok(())
}
