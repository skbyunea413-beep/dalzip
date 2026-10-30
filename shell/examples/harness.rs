//! Drives dz_shell.dll the way Explorer does: CoCreateInstance through the
//! registry, IInitializeCommand with the verb name, an IShellItemArray of real
//! paths, then GetTitle / GetState / GetIcon and optionally Invoke.
//!
//! usage: harness <verb> <invoke:0|1> <path>...

use std::ptr::null_mut;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows_core::*;

const CLSID_DALZIP: GUID = GUID::from_u128(0x2BEFAA5D_6E6D_44ED_BF55_CE6F77683D4B);

fn take(p: PWSTR) -> String {
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (verb, invoke, paths) = (&args[0], args[1] == "1", &args[2..]);
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let step = |name: &str, r: Result<()>| -> Result<()> {
            r.map_err(|e| {
                eprintln!("{name} failed: {e:?}");
                e
            })
        };
        let cmd: IExplorerCommand = CoCreateInstance(&CLSID_DALZIP, None, CLSCTX_INPROC_SERVER).inspect_err(|e| eprintln!("CoCreateInstance failed: {e:?}"))?;
        let init: IInitializeCommand = cmd.cast()?;
        step("Initialize", init.Initialize(&HSTRING::from(verb.as_str()), None))?;

        let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
        for p in paths {
            let mut pidl: *mut ITEMIDLIST = null_mut();
            SHParseDisplayName(&HSTRING::from(p.as_str()), None, &mut pidl, 0, None)?;
            pidls.push(pidl);
        }
        let items = SHCreateShellItemArrayFromIDLists(&pidls)?;

        let title = take(cmd.GetTitle(&items).inspect_err(|e| eprintln!("GetTitle failed: {e:?}"))?);
        let state = cmd.GetState(&items, false.into()).inspect_err(|e| eprintln!("GetState failed: {e:?}"))?;
        let icon = take(cmd.GetIcon(&items).inspect_err(|e| eprintln!("GetIcon failed: {e:?}"))?);
        println!("verb={verb} title=\"{title}\" state={} icon={icon}", if state == 0 { "shown" } else { "hidden" });
        if invoke {
            cmd.Invoke(&items, None)?;
            println!("invoked");
        }
        for p in pidls {
            CoTaskMemFree(Some(p as *const _));
        }
    }
    Ok(())
}
