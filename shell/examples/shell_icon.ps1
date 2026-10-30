# Saves the icon the shell shows for a file (SHGetFileInfo, the lookup Explorer uses) as a PNG,
# plus the embedded resource `-Index` of an exe. Run outside the Claude app container.
# usage: pwsh shell_icon.ps1 -File <path> -Out <png> [-Exe <exe> -Index <negative id> -ExeOut <png>]
param([string]$File, [string]$Out, [string]$Exe, [int]$Index, [string]$ExeOut)
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class SI {
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  public struct SHFILEINFO { public IntPtr hIcon; public int iIcon; public uint attr;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string name;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 80)] public string type; }
  [DllImport("shell32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SHGetFileInfo(string p, uint a, ref SHFILEINFO i, uint s, uint f);
  [DllImport("shell32.dll", CharSet = CharSet.Unicode)] public static extern int ExtractIconEx(string f, int i, IntPtr[] large, IntPtr[] small, int n);
  [DllImport("user32.dll")] public static extern bool DestroyIcon(IntPtr h);
}
'@
function Save([IntPtr]$h, [string]$png) {
    $ico = [System.Drawing.Icon]::FromHandle($h)
    $ico.ToBitmap().Save($png)
    [void][SI]::DestroyIcon($h)
}
$info = New-Object SI+SHFILEINFO
[void][SI]::SHGetFileInfo($File, 0, [ref]$info, [System.Runtime.InteropServices.Marshal]::SizeOf($info), 0x100 -bor 0x400)  # SHGFI_ICON | SHGFI_TYPENAME
"type name: $($info.type)"
Save $info.hIcon $Out
if ($Exe) {
    $large = New-Object IntPtr[] 1
    $n = [SI]::ExtractIconEx($Exe, $Index, $large, $null, 1)
    "ExtractIconEx $Index -> $n icon(s)"
    if ($n -gt 0) { Save $large[0] $ExeOut }
}
