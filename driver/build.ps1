# Build the Crossfade virtual audio driver package (driver + setup tool).
#
#   .\driver\build.ps1                     # Release, x64
#   .\driver\build.ps1 -Platform ARM64 -Configuration Debug
#
# Needs Visual Studio 2022 (or Build Tools) with the C++ workload, the
# Spectre-mitigated libraries and the Windows Driver Kit component. The
# package lands in driver\build\<Platform>\<Configuration>\CrossfadeAudio\,
# test-signed with the WDK's test certificate.
param(
    [ValidateSet('x64', 'ARM64')] [string] $Platform = 'x64',
    [ValidateSet('Release', 'Debug')] [string] $Configuration = 'Release'
)
$ErrorActionPreference = 'Stop'

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$msbuild = & $vswhere -latest -products * -requires Microsoft.Component.MSBuild `
    -find 'MSBuild\**\Bin\MSBuild.exe' | Select-Object -First 1
if (-not $msbuild) { throw 'MSBuild not found; install Visual Studio 2022 or its Build Tools.' }

foreach ($project in 'CrossfadeAudio.vcxproj', 'setup\setup.vcxproj') {
    & $msbuild (Join-Path $PSScriptRoot $project) /nologo /v:minimal `
        "/p:Configuration=$Configuration" "/p:Platform=$Platform"
    if ($LASTEXITCODE -ne 0) { throw "$project failed to build" }
}

$package = Join-Path $PSScriptRoot "build\$Platform\$Configuration\CrossfadeAudio"
# The test certificate, for importing on test machines (see README).
Copy-Item (Join-Path $PSScriptRoot "build\$Platform\$Configuration\CrossfadeAudio.cer") $package
Write-Host "Driver package: $package"
