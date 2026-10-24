# Build the Windows installer: GleemCrossfade-<version>-setup.exe.
#
#   .\packaging\windows\build-installer.ps1
#
# Builds the app (release) and the driver package, stages everything the
# app needs at runtime, and compiles crossfade.iss with Inno Setup 6.
#
# Requirements: Rust, the gvsbuild GTK4 bundle in C:\gtk (or -GtkRoot),
# Visual Studio 2022 Build Tools with the C++ workload and the WDK (for the
# driver and dumpbin), and Inno Setup 6.
param(
    [string] $GtkRoot = 'C:\gtk',
    [switch] $SkipBuild
)
$ErrorActionPreference = 'Stop'

$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$out = Join-Path $PSScriptRoot 'build'
$stage = Join-Path $out 'stage'
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

# ---- Tools -------------------------------------------------------------------
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'Visual Studio 2022 (or Build Tools) with the C++ workload is required.' }
$dumpbin = Get-ChildItem "$vs\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" | Select-Object -Last 1
$crt = Get-ChildItem "$vs\VC\Redist\MSVC\*\x64\Microsoft.VC143.CRT" -Directory | Select-Object -Last 1
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
          "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
          "$env:ProgramFiles\Inno Setup 6\ISCC.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $dumpbin) { throw 'dumpbin.exe not found.' }
if (-not $crt) { throw 'Visual C++ runtime redistributable folder not found.' }
if (-not $iscc) { throw 'Inno Setup 6 (ISCC.exe) not found: winget install JRSoftware.InnoSetup' }

# ---- Build -------------------------------------------------------------------
if (-not $SkipBuild) {
    $env:PKG_CONFIG_PATH = "$GtkRoot\lib\pkgconfig"
    $env:Path = "$GtkRoot\bin;$env:Path"
    $env:LIB = "$GtkRoot\lib;$env:LIB"
    Push-Location $root
    try {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally {
        Pop-Location
    }
    & (Join-Path $root 'driver\build.ps1')
}
$exe = Join-Path $root 'target\release\gleem-crossfade.exe'
$driver = Join-Path $root 'driver\build\x64\Release\CrossfadeAudio'

# ---- Stage -------------------------------------------------------------------
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
$bin = New-Item -ItemType Directory -Force (Join-Path $stage 'bin')

# Copy a binary plus every DLL it (transitively) needs from the GTK bundle.
# DLLs not found there are Windows' own (or the C runtime, bundled below).
$copied = @{}
function Add-WithDependencies([string] $path, [string] $dest) {
    $name = [IO.Path]::GetFileName($path).ToLowerInvariant()
    if ($copied.ContainsKey($name)) { return }
    $copied[$name] = $true
    Copy-Item $path $dest
    $deps = & $dumpbin.FullName /nologo /dependents $path |
        Where-Object { $_ -match '^\s+(\S+\.dll)\s*$' } | ForEach-Object { $Matches[1] }
    foreach ($dep in $deps) {
        $candidate = Join-Path "$GtkRoot\bin" $dep
        if (Test-Path $candidate) { Add-WithDependencies $candidate $bin }
    }
}

Add-WithDependencies $exe $bin
# GLib starts gdbus.exe as the session bus (single instance, --quit, D-Bus API).
Add-WithDependencies "$GtkRoot\bin\gdbus.exe" $bin
# The SVG image loader (symbolic icons, tray icon); its own dependencies go
# to bin\ like all others. loaders.cache refers to it by a relative path.
$loaders = New-Item -ItemType Directory -Force (Join-Path $stage 'lib\gdk-pixbuf-2.0\2.10.0\loaders')
Copy-Item "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders.cache" $loaders.Parent.FullName
Add-WithDependencies "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders\pixbufloader_svg.dll" $loaders

# The Visual C++ runtime, deployed app-locally so a fresh Windows works.
Copy-Item "$($crt.FullName)\*.dll" $bin

# GSettings schemas and icon themes, found relative to bin\ at runtime.
$schemas = New-Item -ItemType Directory -Force (Join-Path $stage 'share\glib-2.0\schemas')
Copy-Item "$GtkRoot\share\glib-2.0\schemas\gschemas.compiled" $schemas
$icons = New-Item -ItemType Directory -Force (Join-Path $stage 'share\icons')
Copy-Item -Recurse "$GtkRoot\share\icons\Adwaita" $icons
Copy-Item -Recurse "$GtkRoot\share\icons\hicolor" $icons
$appIcons = New-Item -ItemType Directory -Force (Join-Path $icons 'hicolor\scalable\apps')
Copy-Item (Join-Path $root 'data\icons\hicolor\scalable\apps\gg.gleem.Crossfade.svg') $appIcons

# The driver package and its setup tool.
$driverStage = New-Item -ItemType Directory -Force (Join-Path $stage 'driver')
foreach ($file in 'CrossfadeAudio.sys', 'CrossfadeAudio.inf', 'crossfadeaudio.cat',
                  'crossfade-driver-setup.exe', 'CrossfadeAudio.cer') {
    Copy-Item (Join-Path $driver $file) $driverStage
}

Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')

$size = (Get-ChildItem $stage -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
Write-Host ("Staged {0} files, {1:N1} MB" -f (Get-ChildItem $stage -Recurse -File).Count, $size)

# ---- Installer -------------------------------------------------------------------
& $iscc /Qp "/DAppVersion=$version" "/DStage=$stage" "/O$out" (Join-Path $PSScriptRoot 'crossfade.iss')
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup failed' }
Write-Host "Installer: $(Join-Path $out "GleemCrossfade-$version-setup.exe")"
