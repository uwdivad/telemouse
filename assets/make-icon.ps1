# Regenerates assets/telemouse.ico, the application icon every build.rs puts
# on its executable.
#
#   pwsh assets/make-icon.ps1            (Windows PowerShell 5.1 works too)
#
# The picture is the tray icon's identity (crates/ctl/src/gui/model.rs,
# `icon_bitmap`: a filled disc with a darker ring) in the panel's colours:
# the accent #35d0e0 on a rounded tile of the panel's dark #0b0f16. From
# 32 px up a short fading trail behind the disc says "pointer in motion";
# below that it is only noise, so the small sizes are the disc alone.
#
# Container: sizes below 256 are classic 32-bit DIB entries (BITMAPINFOHEADER,
# bottom-up BGRA, then a 1-bit AND mask), which every resource compiler and
# every icon reader understands; 256 is a PNG entry, as Windows itself does.
# The header is written by hand because System.Drawing's Icon.Save only ever
# writes one low-colour image.

param([string]$Out = (Join-Path $PSScriptRoot 'telemouse.ico'))

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$sizes  = 16, 24, 32, 48, 64, 256
$tile   = [System.Drawing.ColorTranslator]::FromHtml('#0b0f16')
$edge   = [System.Drawing.ColorTranslator]::FromHtml('#1c2533')   # 1-px tile outline, keeps the tile visible on dark taskbars
$accent = [System.Drawing.ColorTranslator]::FromHtml('#35d0e0')
$ring   = [System.Drawing.ColorTranslator]::FromHtml('#1f8794')   # the accent darkened, like the tray disc's ring

function New-RoundedRect([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = 2 * $r
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    $p
}

function New-IconBitmap([int]$n) {
    $bmp = New-Object System.Drawing.Bitmap $n, $n, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'HighQuality'
    $g.Clear([System.Drawing.Color]::Transparent)

    # The tile.
    $inset = 0.5
    $path = New-RoundedRect $inset $inset ($n - 2 * $inset) ($n - 2 * $inset) ($n * 0.22)
    $b = New-Object System.Drawing.SolidBrush $tile
    $g.FillPath($b, $path); $b.Dispose()
    $pen = New-Object System.Drawing.Pen $edge, ([single][Math]::Max(1.0, $n / 64.0))
    $g.DrawPath($pen, $path); $pen.Dispose(); $path.Dispose()

    $trail = $n -ge 32
    # The disc sits up and to the right when it has a trail to lead.
    $cx = if ($trail) { $n * 0.60 } else { $n * 0.5 }
    $cy = if ($trail) { $n * 0.42 } else { $n * 0.5 }
    $r  = if ($trail) { $n * 0.235 } else { $n * 0.30 }

    if ($trail) {
        # Three shrinking, fading dots along the path the disc came by.
        $steps = @(
            @{ t = 0.31; k = 0.52; a = 150 },
            @{ t = 0.49; k = 0.36; a = 95 },
            @{ t = 0.62; k = 0.24; a = 55 }
        )
        foreach ($s in $steps) {
            $px = $cx - $n * $s.t * 0.80
            $py = $cy + $n * $s.t * 0.62
            $pr = $r * $s.k
            $tb = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb($s.a, $accent))
            $g.FillEllipse($tb, [single]($px - $pr), [single]($py - $pr), [single](2 * $pr), [single](2 * $pr))
            $tb.Dispose()
        }
    }

    # The disc: darker ring outside, accent fill inside.
    $rb = New-Object System.Drawing.SolidBrush $ring
    $g.FillEllipse($rb, [single]($cx - $r), [single]($cy - $r), [single](2 * $r), [single](2 * $r)); $rb.Dispose()
    $ri = $r - [Math]::Max(1.0, $n * 0.045)
    $ab = New-Object System.Drawing.SolidBrush $accent
    $g.FillEllipse($ab, [single]($cx - $ri), [single]($cy - $ri), [single](2 * $ri), [single](2 * $ri)); $ab.Dispose()

    $g.Dispose()
    $bmp
}

function Get-PngBytes($bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    , $ms.ToArray()
}

function Get-DibBytes($bmp) {
    $n = $bmp.Width
    $ms = New-Object System.IO.MemoryStream
    $w = New-Object System.IO.BinaryWriter $ms
    # BITMAPINFOHEADER; the height counts the colour bitmap and the mask.
    $w.Write([uint32]40); $w.Write([int32]$n); $w.Write([int32](2 * $n))
    $w.Write([uint16]1); $w.Write([uint16]32); $w.Write([uint32]0)
    $w.Write([uint32]0); $w.Write([int32]0); $w.Write([int32]0); $w.Write([uint32]0); $w.Write([uint32]0)
    $maskStride = [int]([Math]::Ceiling($n / 32.0) * 4)
    $mask = New-Object byte[] ($maskStride * $n)
    for ($y = $n - 1; $y -ge 0; $y--) {
        for ($x = 0; $x -lt $n; $x++) {
            $c = $bmp.GetPixel($x, $y)
            $w.Write([byte]$c.B); $w.Write([byte]$c.G); $w.Write([byte]$c.R); $w.Write([byte]$c.A)
            if ($c.A -eq 0) {
                $row = $n - 1 - $y
                $i = $row * $maskStride + [int][Math]::Floor($x / 8)
                $mask[$i] = $mask[$i] -bor (0x80 -shr ($x % 8))
            }
        }
    }
    $w.Write($mask)
    $w.Flush()
    , $ms.ToArray()
}

$images = foreach ($n in $sizes) {
    $bmp = New-IconBitmap $n
    $bytes = if ($n -ge 256) { Get-PngBytes $bmp } else { Get-DibBytes $bmp }
    $bmp.Dispose()
    [pscustomobject]@{ Size = $n; Bytes = $bytes }
}

$ms = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $ms
# ICONDIR
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$images.Count)
$offset = 6 + 16 * $images.Count
foreach ($im in $images) {
    # ICONDIRENTRY; a width/height byte of 0 means 256.
    $dim = if ($im.Size -ge 256) { 0 } else { $im.Size }
    $w.Write([byte]$dim); $w.Write([byte]$dim); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32)
    $w.Write([uint32]$im.Bytes.Length); $w.Write([uint32]$offset)
    $offset += $im.Bytes.Length
}
foreach ($im in $images) { $w.Write([byte[]]$im.Bytes) }
$w.Flush()
[System.IO.File]::WriteAllBytes($Out, $ms.ToArray())
"wrote $Out ($($ms.Length) bytes; sizes $($sizes -join ', '))"
