param(
    [string] $Iscc = (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
)

$ErrorActionPreference = 'Stop'
if (!(Test-Path -LiteralPath $Iscc -PathType Leaf)) {
    $Iscc = (Get-Command ISCC.exe).Source
}

Push-Location (Join-Path $PSScriptRoot '..')
try {
    $target = 'x86_64-pc-windows-msvc'
    $dist = Join-Path (Get-Location).Path 'target\dist'
    $exe = Join-Path $dist "$target\release\igorio.exe"
    $fallback = Join-Path $dist 'igorio-no-ui-access.exe'

    cargo build --locked -p igorio-app --release --target $target --target-dir $dist
    if ($LASTEXITCODE -ne 0) { throw 'The fallback build failed.' }
    Copy-Item -LiteralPath $exe -Destination $fallback -Force

    cargo build --locked -p igorio-app --release --features ui-access --target $target --target-dir $dist
    if ($LASTEXITCODE -ne 0) { throw 'The UI access build failed.' }

    $version = (Get-Item -LiteralPath $exe).VersionInfo.ProductVersion
    & $Iscc "/DIgorioVersion=$version" "/DIgorioExe=$exe" "/DFallbackExe=$fallback" `
        "/O$dist\installer" (Join-Path $PSScriptRoot 'Igorio.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed.' }
} finally {
    Pop-Location
}
