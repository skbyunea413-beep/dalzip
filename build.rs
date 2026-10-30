/// File-type icons embedded after the app icon, with resource IDs 101, 102, …
/// in this order; `dzui.exe,-101` is the .dz icon (see src/bin/dzui/register.rs).
/// Regenerate them with assets/make_type_icons.ps1.
const TYPE_ICONS: &[&str] = &[
    "dz", "zip", "zipx", "7z", "rar", "alz", "egg", "tar", "gz", "tgz", "bz2", "xz", "zst", "lz4", "cab", "iso", "lzh", "zpaq", "001",
];

fn main() {
    // Icon for dz.exe and dzui.exe; dzui.exe,0 is also the Explorer menu icon.
    println!("cargo:rerun-if-changed=assets/dalzip.ico");
    println!("cargo:rerun-if-changed=assets/types");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/dalzip.ico");
        for (i, ext) in TYPE_ICONS.iter().enumerate() {
            res.set_icon_with_id(&format!("assets/types/{ext}.ico"), &(101 + i).to_string());
        }
        res.set("FileDescription", "dalzip");
        res.set("ProductName", "dalzip");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icons not embedded: {e}");
        }
    }
}
