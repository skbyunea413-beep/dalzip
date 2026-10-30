# Compares dz against Bandizip's bz.exe on one folder.
# usage: pwsh bench\bench.ps1 -Corpus <folder> -Work <scratch folder>
param(
    [Parameter(Mandatory)] [string] $Corpus,
    [Parameter(Mandatory)] [string] $Work,
    [string] $Bz = 'C:\Program Files\Bandizip\bz.exe',
    [string] $Dz = (Join-Path $PSScriptRoot '..\target\release\dz.exe')
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $Work | Out-Null
$raw = (Get-ChildItem $Corpus -Recurse -File | Measure-Object Length -Sum).Sum

function Run($name, $archive, [scriptblock] $pack, [scriptblock] $unpack) {
    Remove-Item -Force $archive -ErrorAction SilentlyContinue
    $out = Join-Path $Work 'unpacked'
    Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
    $c = Measure-Command { & $pack | Out-Null }
    $d = Measure-Command { & $unpack $out | Out-Null }
    $size = (Get-Item $archive).Length
    [pscustomobject]@{
        Tool       = $name
        'Size MB'  = [math]::Round($size / 1MB, 2)
        'Ratio %'  = [math]::Round($size * 100 / $raw, 1)
        'Pack s'   = [math]::Round($c.TotalSeconds, 1)
        'Unpack s' = [math]::Round($d.TotalSeconds, 1)
    }
}

$rows = @()
foreach ($l in 5, 9) {
    $a = Join-Path $Work "bz$l.zip"
    $rows += Run "Bandizip zip -l:$l" $a { & $Bz c -y "-l:$l" $a $Corpus } { param($o) & $Bz x -y "-o:$o" $a }
}
foreach ($l in 5, 9) {
    $a = Join-Path $Work "bz$l.7z"
    $rows += Run "Bandizip 7z -l:$l" $a { & $Bz c -y -fmt:7z "-l:$l" $a $Corpus } { param($o) & $Bz x -y "-o:$o" $a }
}
foreach ($l in 1, 3, 4, 5, 6, 7, 9) {
    $a = Join-Path $Work "dz$l.dz"
    $rows += Run "dz -l $l" $a { & $Dz c -q -l $l $a $Corpus } { param($o) & $Dz x -q -y -o $o $a }
}
"raw: {0:N1} MB" -f ($raw / 1MB)
$rows | Format-Table -AutoSize
