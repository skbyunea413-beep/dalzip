# Renders one .ico per archive extension into assets\types\ (16/24/32/48/64/256 px, PNG-in-ICO):
# a rounded tile in the format's symbolic colour, dalzip's crescent moon, and a dark badge with
# the extension (Bandizip's icon layout, dalzip's own artwork). Also writes types-preview.png.
# usage: pwsh assets\make_type_icons.ps1
Add-Type -AssemblyName System.Drawing
$outDir = Join-Path $PSScriptRoot 'types'
New-Item -ItemType Directory -Force $outDir | Out-Null

# ext, tile colour (symbolic), moon colour
$types = @(
    @('dz',   '#3949AB', '#FFD54F'),   # dalzip: indigo night, yellow moon
    @('zip',  '#F9A825', '#FFFFFF'),   # the classic yellow zip folder
    @('zipx', '#EF6C00', '#FFFFFF'),
    @('7z',   '#37474F', '#FFFFFF'),   # 7-Zip's black and white
    @('rar',  '#7B1FA2', '#FFFFFF'),   # WinRAR's purple books
    @('alz',  '#039BE5', '#FFFFFF'),   # ALZip blue
    @('egg',  '#F4511E', '#FFF3E0'),
    @('tar',  '#6D4C41', '#FFFFFF'),   # tape-archive brown
    @('gz',   '#2E7D32', '#FFFFFF'),   # gzip green
    @('tgz',  '#388E3C', '#FFFFFF'),
    @('bz2',  '#00897B', '#FFFFFF'),
    @('xz',   '#0277BD', '#FFFFFF'),
    @('zst',  '#C62828', '#FFFFFF'),   # zstd red
    @('lz4',  '#AD1457', '#FFFFFF'),
    @('cab',  '#5C6BC0', '#FFFFFF'),
    @('iso',  '#78909C', '#FFFFFF'),   # silver disc
    @('lzh',  '#827717', '#FFFFFF'),
    @('zpaq', '#BF360C', '#FFFFFF'),
    @('001',  '#546E7A', '#FFFFFF')    # split volumes
)

function Col([string]$hex, [int]$a = 255) { $c = [System.Drawing.ColorTranslator]::FromHtml($hex); [System.Drawing.Color]::FromArgb($a, $c.R, $c.G, $c.B) }
function Lighter($c, [double]$k) { [System.Drawing.Color]::FromArgb($c.A, [int]($c.R + (255 - $c.R) * $k), [int]($c.G + (255 - $c.G) * $k), [int]($c.B + (255 - $c.B) * $k)) }
function Darker($c, [double]$k) { [System.Drawing.Color]::FromArgb($c.A, [int]($c.R * (1 - $k)), [int]($c.G * (1 - $k)), [int]($c.B * (1 - $k))) }

function RoundRect([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $p.AddArc($x, $y, 2 * $r, 2 * $r, 180, 90)
    $p.AddArc($x + $w - 2 * $r, $y, 2 * $r, 2 * $r, 270, 90)
    $p.AddArc($x + $w - 2 * $r, $y + $h - 2 * $r, 2 * $r, 2 * $r, 0, 90)
    $p.AddArc($x, $y + $h - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $p.CloseFigure()
    $p
}

function Render([string]$ext, [string]$tileHex, [string]$moonHex, [int]$n) {
    $bmp = New-Object System.Drawing.Bitmap $n, $n
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'; $g.TextRenderingHint = 'AntiAliasGridFit'
    $g.Clear([System.Drawing.Color]::Transparent)
    $base = Col $tileHex
    $small = $n -lt 32
    # Tile: full size when small (the colour alone identifies it), upper-left when a badge follows.
    $ts = if ($small) { [single]($n * 0.92) } else { [single]($n * 0.80) }
    $tx = [single]($n * 0.04); $ty = [single]($n * 0.04)
    $tile = RoundRect $tx $ty $ts $ts ([single]($ts * 0.22))
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.PointF $tx, $ty), (New-Object System.Drawing.PointF ($tx + $ts), ($ty + $ts)), (Lighter $base 0.25), (Darker $base 0.12)
    $g.FillPath($grad, $tile)
    # Crescent moon in the tile's upper part.
    $d = [single]($ts * 0.56); $mx = [single]($tx + $ts * 0.16); $my = [single]($ty + $ts * 0.12)
    $moon = New-Object System.Drawing.Drawing2D.GraphicsPath; $moon.AddEllipse($mx, $my, $d, $d)
    $cut = New-Object System.Drawing.Drawing2D.GraphicsPath; $cut.AddEllipse([single]($mx + $d * 0.36), [single]($my - $d * 0.12), $d, $d)
    $region = New-Object System.Drawing.Region $moon; $region.Exclude($cut)
    $g.FillRegion((New-Object System.Drawing.SolidBrush (Col $moonHex)), $region)
    if (-not $small) {
        # Dark badge with the extension, overlapping the tile's lower right.
        $bw = [single]($n * 0.70); $bh = [single]($n * 0.34); $bx = [single]($n - $bw - $n * 0.02); $by = [single]($n - $bh - $n * 0.03)
        $badge = RoundRect $bx $by $bw $bh ([single]($bh * 0.28))
        $g.FillPath((New-Object System.Drawing.SolidBrush (Col '#263238')), $badge)
        $g.DrawPath((New-Object System.Drawing.Pen (Col '#FFFFFF' 230), ([single][Math]::Max(1, $n / 64))), $badge)
        $label = $ext.ToUpper()
        $fs = [single]($bh * 0.66)
        $font = New-Object System.Drawing.Font 'Segoe UI', $fs, ([System.Drawing.FontStyle]::Bold), ([System.Drawing.GraphicsUnit]::Pixel)
        while ($g.MeasureString($label, $font).Width -gt $bw * 0.92 -and $fs -gt 4) {
            $fs = [single]($fs * 0.92); $font = New-Object System.Drawing.Font 'Segoe UI', $fs, ([System.Drawing.FontStyle]::Bold), ([System.Drawing.GraphicsUnit]::Pixel)
        }
        $fmt = New-Object System.Drawing.StringFormat; $fmt.Alignment = 'Center'; $fmt.LineAlignment = 'Center'
        $g.DrawString($label, $font, (New-Object System.Drawing.SolidBrush (Col '#FFFFFF')), (New-Object System.Drawing.RectangleF $bx, ($by + $bh * 0.03), $bw, $bh), $fmt)
        # A stripe of the format colour on the badge, so even a glance at the corner tells formats apart.
        $g.FillRectangle((New-Object System.Drawing.SolidBrush $base), [single]($bx + $bh * 0.28), [single]($by + $bh - [Math]::Max(1, $n / 40)), [single]($bw - $bh * 0.56), [single][Math]::Max(1, $n / 40))
    }
    $g.Dispose()
    $bmp
}

function ToPng($bmp) { $ms = New-Object System.IO.MemoryStream; $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); , $ms.ToArray() }

$sizes = 16, 24, 32, 48, 64, 256
foreach ($t in $types) {
    $pngs = @(foreach ($s in $sizes) { $b = Render $t[0] $t[1] $t[2] $s; , (ToPng $b); $b.Dispose() })
    $fs = [System.IO.File]::Create((Join-Path $outDir "$($t[0]).ico"))
    $w = New-Object System.IO.BinaryWriter $fs
    $w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $s = $sizes[$i]; $px = if ($s -ge 256) { 0 } else { $s }
        $w.Write([byte]$px); $w.Write([byte]$px); $w.Write([byte]0); $w.Write([byte]0)
        $w.Write([uint16]1); $w.Write([uint16]32); $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
        $offset += $pngs[$i].Length
    }
    foreach ($p in $pngs) { $w.Write($p) }
    $w.Close()
}

# Preview: every type at 128, 48, 32 and 16 px on a dark and a light strip.
$cols = 10; $cell = 150
$rows = [Math]::Ceiling($types.Count / $cols)
$sheet = New-Object System.Drawing.Bitmap ($cols * $cell), ($rows * $cell * 2)
$sg = [System.Drawing.Graphics]::FromImage($sheet)
$sg.Clear((Col '#2B2B2B'))
$sg.FillRectangle((New-Object System.Drawing.SolidBrush (Col '#F3F3F3')), 0, $rows * $cell, $cols * $cell, $rows * $cell)
for ($bg = 0; $bg -lt 2; $bg++) {
    for ($i = 0; $i -lt $types.Count; $i++) {
        $x = ($i % $cols) * $cell; $y = [Math]::Floor($i / $cols) * $cell + $bg * $rows * $cell
        $big = Render $types[$i][0] $types[$i][1] $types[$i][2] 96; $sg.DrawImage($big, $x + 8, $y + 6); $big.Dispose()
        $px = $x + 8
        foreach ($s in 48, 32, 16) { $b = Render $types[$i][0] $types[$i][1] $types[$i][2] $s; $sg.DrawImage($b, $px, $y + 106 + (48 - $s)); $px += $s + 6; $b.Dispose() }
    }
}
$sheet.Save((Join-Path $PSScriptRoot 'types-preview.png'))
"wrote $($types.Count) icons to $outDir"
