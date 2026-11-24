param(
    [Parameter(Mandatory)]
    [string] $Executable,
    [Parameter(Mandatory)]
    [string] $Version
)

$ErrorActionPreference = 'Stop'
$KarryFixtureBinary = (Resolve-Path -LiteralPath $Executable).Path
$KarryFixtureHash = (Get-FileHash -LiteralPath $KarryFixtureBinary -Algorithm SHA256).Hash.ToLowerInvariant()
$KarryFixtureTag = "v$Version"

function Invoke-WebRequest {
    param([string] $Uri, [switch] $UseBasicParsing, [string] $OutFile)
    $base = 'https://github.com/harzer228/KarryCode/releases'
    $latest = "$base/latest/download"
    $pinned = "$base/download/$KarryFixtureTag"
    if ($Uri -cin @("$latest/SHA256SUMS", "$pinned/SHA256SUMS")) {
        Set-Content -LiteralPath $OutFile -Encoding ascii -Value "$KarryFixtureHash  karrycode-windows-x64.exe"
    } elseif ($Uri -cin @("$latest/karrycode-windows-x64.exe", "$pinned/karrycode-windows-x64.exe")) {
        Copy-Item -LiteralPath $KarryFixtureBinary -Destination $OutFile
    } else {
        throw "Unexpected asset URL: $Uri"
    }
}

$oldPath = $env:Path
$oldLocal = $env:LOCALAPPDATA
$local = Join-Path ([IO.Path]::GetTempPath()) ('karrycode-installer-fixture-' + [Guid]::NewGuid().ToString('N'))
$repoRoot = if ($PSScriptRoot) { Split-Path -Parent $PSScriptRoot } else { (Get-Location).Path }
$installer = [scriptblock]::Create((Get-Content -LiteralPath (Join-Path $repoRoot 'install.ps1') -Raw))
$env:LOCALAPPDATA = $local

try {
    $data = Join-Path $local 'KarryCode\profile'
    $null = New-Item -ItemType Directory -Path $data -Force
    $profile = Join-Path $data 'snapshot-0.age'
    Set-Content -LiteralPath $profile -Value 'synthetic profile'

    & $installer -NoUserPath
    $expected = Join-Path $local 'Programs\KarryCode\bin\karrycode.exe'
    if ((Get-Command karrycode -CommandType Application).Source -cne $expected) {
        throw 'The installed karrycode was not on the current PATH.'
    }
    if ((& karrycode --version) -cne "KarryCode CLI $Version") { throw 'Unexpected version.' }
    & $installer -Version $KarryFixtureTag -NoUserPath
    $original = (Get-FileHash -LiteralPath $expected -Algorithm SHA256).Hash
    $KarryFixtureHash = '0' * 64
    $rejected = $false
    try {
        & $installer -NoUserPath
    } catch {
        if ($_ -notmatch 'SHA-256 mismatch') { throw }
        $rejected = $true
    }
    if (!$rejected -or (Get-FileHash -LiteralPath $expected -Algorithm SHA256).Hash -cne $original) {
        throw 'The invalid update was not rejected safely.'
    }
    & $installer -Uninstall -NoUserPath
    if ((Test-Path -LiteralPath $expected) -or (Get-Content -LiteralPath $profile) -cne 'synthetic profile') {
        throw 'Uninstall did not preserve test data.'
    }
    Write-Output 'Installer smoke test passed.'
} finally {
    $env:Path = $oldPath
    $env:LOCALAPPDATA = $oldLocal
}
