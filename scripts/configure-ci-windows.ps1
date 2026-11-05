$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path $PSScriptRoot -Parent
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -or -not $installation) {
    throw 'MSVC installation not found'
}
$vcvars = Join-Path $installation 'VC/Auxiliary/Build/vcvars64.bat'
# Keep activation and Cargo in the same process so C dependencies see MSVC.
& cmd.exe /d /s /c "call `"$vcvars`" && cargo install mbx --version 1.27.0 --locked --root `"$repoRoot\.ci\tools`""
if ($LASTEXITCODE) { exit $LASTEXITCODE }
