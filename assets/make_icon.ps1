# Renders assets\dalzip.ico (16/24/32/48/64/256 px, PNG-in-ICO): blue rounded square with a crescent moon.
# usage: pwsh assets\make_icon.ps1
Add-Type -AssemblyName System.Drawing
$out = Join-Path $PSScriptRoot 'dalzip.ico'

function Render([int]$n) {
    $bmp = New-Object System.Drawing.Bitmap $n, $n
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)
    $r = [single]($n * 0.22); $m = [single]($n * 0.04); $s = [single]($n - 2 * $m)
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($m, $m, 2 * $r, 2 * $r, 180, 90)
    $path.AddArc($m + $s - 2 * $r, $m, 2 * $r, 2 * $r, 270, 90)
    $path.AddArc($m + $s - 2 * $r, $m + $s - 2 * $r, 2 * $r, 2 * $r, 0, 90)
    $path.AddArc($m, $m + $s - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $path.CloseFigure()
    $bg = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.PointF 0, 0), (New-Object System.Drawing.PointF $n, $n), ([System.Drawing.Color]::FromArgb(255, 21, 101, 192)), ([System.Drawing.Color]::FromArgb(255, 66, 165, 245))
    $g.FillPath($bg, $path)
    # Crescent: a disc minus an offset disc.
    $d = [single]($n * 0.56); $x = [single]($n * 0.20); $y = [single]($n * 0.20)
    $moon = New-Object System.Drawing.Drawing2D.GraphicsPath
    $moon.AddEllipse($x, $y, $d, $d)
    $cut = New-Object System.Drawing.Drawing2D.GraphicsPath
    $cut.AddEllipse([single]($x + $d * 0.36), [single]($y - $d * 0.12), $d, $d)
    $region = New-Object System.Drawing.Region $moon
    $region.Exclude($cut)
    $g.FillRegion((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 255, 213, 79))), $region)
    # Zipper teeth along the right edge, the "archive" half of the mark.
    $tooth = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(235, 255, 255, 255))
    $tw = [single]($n * 0.09); $th = [single]($n * 0.055); $tx = [single]($n * 0.70)
    for ($i = 0; $i -lt 5; $i++) {
        $ty = [single]($n * 0.22 + $i * $n * 0.12)
        $off = if ($i % 2) { $tw * 0.6 } else { 0 }
        $g.FillRectangle($tooth, [single]($tx + $off), $ty, $tw, $th)
    }
    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    , $ms.ToArray()
}

$sizes = 16, 24, 32, 48, 64, 256
$pngs = @(foreach ($s in $sizes) { , (Render $s) })
$fs = [System.IO.File]::Create($out)
$w = New-Object System.IO.BinaryWriter $fs
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $w.Write([byte]($(if ($s -ge 256) { 0 } else { $s }))); $w.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))
    $w.Write([byte]0); $w.Write([byte]0); $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
$w.Close()
# A PNG copy for docs, and raw 64x64 RGBA for the GUI window icon (no PNG decoder needed at runtime).
[System.IO.File]::WriteAllBytes((Join-Path $PSScriptRoot 'dalzip-256.png'), $pngs[-1])
$ms = New-Object System.IO.MemoryStream (, $pngs[4])
$bmp = New-Object System.Drawing.Bitmap $ms
$raw = New-Object byte[] (64 * 64 * 4)
for ($y = 0; $y -lt 64; $y++) {
    for ($x = 0; $x -lt 64; $x++) {
        $c = $bmp.GetPixel($x, $y); $i = ($y * 64 + $x) * 4
        $raw[$i] = $c.R; $raw[$i + 1] = $c.G; $raw[$i + 2] = $c.B; $raw[$i + 3] = $c.A
    }
}
[System.IO.File]::WriteAllBytes((Join-Path $PSScriptRoot 'dalzip-64.rgba'), $raw)
"wrote $out"
