# Regenerate gleem-crossfade.ico from the app's SVG icon.
#
#   .\packaging\windows\make-icon.ps1
#
# Renders the SVG with rsvg-convert (part of the gvsbuild GTK bundle) at the
# sizes Windows uses and packs them into one .ico with PNG-compressed
# entries (supported since Windows Vista). The result is committed, because
# build.rs embeds it into the executable.
param(
    [string] $RsvgConvert = 'C:\gtk\bin\rsvg-convert.exe'
)
$ErrorActionPreference = 'Stop'

$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$svg = Join-Path $root 'data\icons\hicolor\scalable\apps\gg.gleem.Crossfade.svg'
$ico = Join-Path $PSScriptRoot 'gleem-crossfade.ico'
$sizes = 16, 20, 24, 32, 40, 48, 64, 256

$tmp = Join-Path ([IO.Path]::GetTempPath()) "crossfade-icon-$PID"
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    $images = foreach ($size in $sizes) {
        $png = Join-Path $tmp "$size.png"
        & $RsvgConvert -w $size -h $size -o $png $svg
        if ($LASTEXITCODE -ne 0) { throw "rsvg-convert failed for $size px" }
        , [IO.File]::ReadAllBytes($png)
    }

    # ICONDIR, then one ICONDIRENTRY per image, then the PNG data.
    $stream = [IO.File]::Create($ico)
    $writer = New-Object IO.BinaryWriter($stream)
    $writer.Write([UInt16]0)              # reserved
    $writer.Write([UInt16]1)              # type: icon
    $writer.Write([UInt16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $dim = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }  # 0 means 256
        $writer.Write([Byte]$dim)         # width
        $writer.Write([Byte]$dim)         # height
        $writer.Write([Byte]0)            # palette size
        $writer.Write([Byte]0)            # reserved
        $writer.Write([UInt16]1)          # color planes
        $writer.Write([UInt16]32)         # bits per pixel
        $writer.Write([UInt32]$images[$i].Length)
        $writer.Write([UInt32]$offset)
        $offset += $images[$i].Length
    }
    foreach ($image in $images) { $writer.Write($image) }
    $writer.Close()
    Write-Host "Wrote $ico ($((Get-Item $ico).Length) bytes, $($sizes.Count) sizes)"
} finally {
    Remove-Item -Recurse -Force $tmp
}
