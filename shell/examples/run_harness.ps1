# End-to-end check of the Explorer menu, run outside the Claude app container
# (where Explorer's view of HKCU lives):
#   1. turn every menu item on (flat layout) and register
#   2. dump the shell's real context menus for several selections
#   3. invoke the new actions through COM and check what they produced
#   4. put the settings back to defaults and register again
# usage: pwsh shell\examples\run_harness.ps1 -Dir <empty scratch folder>
param([Parameter(Mandatory)][string]$Dir)
$rel = (Resolve-Path (Join-Path $PSScriptRoot '..\..\target\release')).Path
$h = Join-Path $rel 'examples\harness.exe'
$md = Join-Path $rel 'examples\menu_dump.exe'
$dz = Join-Path $rel 'dz.exe'
$log = Join-Path $Dir 'check.log'

function Outside([string]$cmdline, [int]$wait = 2) {
    Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = "cmd.exe /c chcp 65001 >nul & $cmdline >> `"$log`" 2>&1" } | Out-Null
    Start-Sleep -Seconds $wait
}

# Fresh fixtures.
Remove-Item -Recurse -Force $Dir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$Dir\src\docs", "$Dir\bg" | Out-Null
Set-Content "$Dir\src\a.txt" ("alpha " * 2000)
Set-Content "$Dir\src\b.txt" ("bravo " * 2000)
Set-Content "$Dir\src\docs\c.md" "# notes"
& $dz c -q "$Dir\one.dz" "$Dir\src\a.txt"
& $dz c -q "$Dir\two.dz" "$Dir\src\a.txt" "$Dir\src\b.txt"
New-Item -ItemType File "$log" | Out-Null

# 1. every item on, flat
$ids = 'x_here','x_auto','x_name','x_here_del','x_auto_del','x_name_del','x_dlg','x_test','x_open','c_dz','c_zip','c_dz_del','c_each','c_each_zip','c_rule','c_dlg','f_cmd'
$adds = ($ids | ForEach-Object { "reg add HKCU\Software\dalzip /v menu_$_ /t REG_DWORD /d 1 /f >nul" }) -join ' & '
Outside "$adds & reg add HKCU\Software\dalzip /v menu_cascade /t REG_DWORD /d 0 /f >nul & `"$rel\dzui.exe`" --register --silent & type `"$rel\dzui-register.log`"" 5

# 2. real menus; keep top-level lines that are ours (not Bandizip's, not nested 7-Zip ones)
function Menu([string]$label, [string]$argv) {
    Add-Content $log "===== $label" -Encoding utf8
    Outside "`"$md`" $argv" 4
}
Menu 'file' "`"$Dir\src\a.txt`""
Menu 'two files' "`"$Dir\src\a.txt`" `"$Dir\src\b.txt`""
Menu 'archive' "`"$Dir\one.dz`""
Menu 'two archives' "`"$Dir\one.dz`" `"$Dir\two.dz`""
Menu 'folder' "`"$Dir\src\docs`""
Menu 'background' "--bg `"$Dir\bg`""

# 3. actions (verb key names as registered: NN_<id>)
Add-Content $log "===== actions" -Encoding utf8
Outside "`"$h`" 10_c_zip 1 `"$Dir\src`"" 4
Outside "`"$h`" 13_c_each_zip 1 `"$Dir\src\a.txt`" `"$Dir\src\b.txt`"" 4
Outside "`"$h`" 14_c_rule 1 `"$Dir\src\docs`"" 4
Copy-Item -Recurse "$Dir\src\docs" "$Dir\deleteme"
Outside "`"$h`" 11_c_dz_del 1 `"$Dir\deleteme`"" 6
Copy-Item "$Dir\two.dz" "$Dir\x2.dz"
Outside "`"$h`" 04_x_auto_del 1 `"$Dir\x2.dz`"" 6

# 4. defaults back
Outside "reg delete HKCU\Software\dalzip /f >nul & `"$rel\dzui.exe`" --register --silent & type `"$rel\dzui-register.log`"" 5

Get-Content $log -Encoding utf8
