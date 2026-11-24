[CmdletBinding()]
param(
    [Alias('Version')]
    [string] $KarryCodeVersion,
    [switch] $Uninstall,
    [switch] $NoUserPath
)

$ErrorActionPreference = 'Stop'
$tagPattern = '^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$'
if ($KarryCodeVersion -and $KarryCodeVersion -notmatch $tagPattern) {
    throw 'Specify a valid release tag, such as v0.1.0-beta.1.'
}
if ($env:OS -ne 'Windows_NT' -or @($env:PROCESSOR_ARCHITEW6432, $env:PROCESSOR_ARCHITECTURE) -notcontains 'AMD64') {
    throw 'KarryCode requires Windows x64.'
}
$repo = 'harzer228/KarryCode'
$bin = Join-Path $env:LOCALAPPDATA 'Programs\KarryCode\bin'
$exe = Join-Path $bin 'karrycode.exe'

function Update-KarryCodePath([string] $Current, [bool] $Remove) {
    $entries = @($Current -split ';' | Where-Object {
        $_ -ne '' -and $_.Trim().Trim('"').TrimEnd('\') -ine $bin.TrimEnd('\')
    })
    if (-not $Remove) { $entries = @($bin) + $entries }
    return ($entries -join ';')
}

foreach ($path in ((Split-Path -Parent $bin), $bin, $exe)) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Refusing to install through a link: $path"
    }
}

if ($Uninstall) {
    if (Test-Path -LiteralPath $exe) { Remove-Item -LiteralPath $exe }
    if (-not $NoUserPath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        [Environment]::SetEnvironmentVariable('Path', (Update-KarryCodePath $userPath $true), 'User')
    }
    $env:Path = Update-KarryCodePath $env:Path $true
    Write-Output 'KarryCode removed. Your profile was kept.'
    return
}

$ref = if ($KarryCodeVersion) { "tags/$KarryCodeVersion" } else { 'latest' }
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/$ref" -Headers @{ 'User-Agent' = 'KarryCode-Installer' }
$tag = [string] $release.tag_name
if ($release.draft -or $tag -notmatch $tagPattern -or ($KarryCodeVersion -and $tag -cne $KarryCodeVersion)) {
    throw 'The release metadata is invalid.'
}
$assetName = 'karrycode-windows-x64.exe'
$base = "https://github.com/$repo/releases/download/$tag"
$expectedDigest = [string] ($release.assets | Where-Object { $_.name -ceq $assetName } | Select-Object -First 1).digest

$null = New-Item -ItemType Directory -Path $bin -Force
$nonce = [Guid]::NewGuid().ToString('N')
$download = Join-Path $bin "$nonce.exe"
$sums = Join-Path $bin "$nonce.sha256"
$backup = Join-Path $bin "$nonce.previous"
try {
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -UseBasicParsing -OutFile $sums
    $checksumLines = @(Get-Content -LiteralPath $sums | Where-Object { $_ -cmatch ('^[a-fA-F0-9]{64}  ' + [regex]::Escape($assetName) + '$') })
    if ($checksumLines.Count -ne 1) { throw "The release has no unique SHA-256 for $assetName." }
    $sha = $checksumLines[0].Substring(0, 64)
    if ($expectedDigest -and $expectedDigest -ine "sha256:$sha") { throw 'The published hashes disagree.' }
    Invoke-WebRequest -Uri "$base/$assetName" -UseBasicParsing -OutFile $download
    if ((Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash -ine $sha) {
        throw 'SHA-256 mismatch. The installed version was preserved.'
    }
    Unblock-File -LiteralPath $download
    $reported = & $download --version
    if ($LASTEXITCODE -ne 0 -or $reported -cne "KarryCode CLI $($tag.Substring(1))") {
        throw 'The downloaded binary does not match the release.'
    }
    if (Test-Path -LiteralPath $exe) {
        [IO.File]::Replace($download, $exe, $backup)
    } else {
        [IO.File]::Move($download, $exe)
    }
} finally {
    foreach ($file in ($download, $sums, $backup)) {
        if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file -ErrorAction SilentlyContinue }
    }
}

if (-not $NoUserPath) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    [Environment]::SetEnvironmentVariable('Path', (Update-KarryCodePath $userPath $false), 'User')
}
$env:Path = Update-KarryCodePath $env:Path $false
Write-Output "Installed KarryCode CLI $($tag.Substring(1)). Run karrycode to get started."
