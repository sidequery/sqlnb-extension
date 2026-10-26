param([string]$BuildDirectory = (Join-Path $env:TEMP ('sqlnb-windows-' + [guid]::NewGuid())))
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$Repo = Split-Path -Parent $PSScriptRoot
$DuckDBRevision = 'd54cbbe3df17e8e5ce68312f483b4155836112aa'
$VcpkgRevision = '84bab45d415d22042bd0b9081aea57f362da3f35'
$DuckDB = Join-Path $BuildDirectory 'duckdb'
$Vcpkg = Join-Path $BuildDirectory 'vcpkg'
$Build = Join-Path $BuildDirectory 'build'
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
foreach ($Checkout in @(@('https://github.com/duckdb/duckdb.git', $DuckDBRevision, $DuckDB), @('https://github.com/microsoft/vcpkg.git', $VcpkgRevision, $Vcpkg))) {
    git init --quiet $Checkout[2]
    git -C $Checkout[2] remote add origin $Checkout[0]
    git -C $Checkout[2] fetch --quiet --depth 1 origin $Checkout[1]
    git -C $Checkout[2] checkout --quiet --detach FETCH_HEAD
    if ((git -C $Checkout[2] rev-parse HEAD) -ne $Checkout[1]) { throw 'Dependency revision mismatch' }
}
& (Join-Path $Vcpkg 'bootstrap-vcpkg.bat') -disableMetrics
& (Join-Path $Vcpkg 'vcpkg.exe') install openssl:x64-windows-static zlib:x64-windows-static
cmake -S $DuckDB -B $Build -G Ninja -DCMAKE_BUILD_TYPE=Release `
    "-DCMAKE_TOOLCHAIN_FILE=$Vcpkg/scripts/buildsystems/vcpkg.cmake" `
    -DVCPKG_MANIFEST_MODE=OFF -DVCPKG_TARGET_TRIPLET=x64-windows-static `
    "-DDUCKDB_EXTENSION_CONFIGS=$Repo/extension_config_release.cmake" `
    -DDUCKDB_EXPLICIT_VERSION=v2.0.0 -DDUCKDB_EXPLICIT_PLATFORM=windows_amd64 `
    -DBUILD_UNITTESTS=OFF -DBUILD_SHELL=ON -DEXTENSION_STATIC_BUILD=OFF
cmake --build $Build --target shell quack_loadable_extension sqlnb_loadable_extension --parallel 2
bun "$Repo/scripts/windows-runtime-smoke.mjs" `
    --duckdb "$Build/duckdb.exe" --extension "$Build/extension/sqlnb/sqlnb.duckdb_extension" `
    --quack-extension "$Build/extension/quack/quack.duckdb_extension"
