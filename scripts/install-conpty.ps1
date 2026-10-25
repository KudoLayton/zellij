#requires -Version 7.0
<#
.SYNOPSIS
Installs an explicitly selected Microsoft ConPTY pair without downloading it.
.DESCRIPTION
Requires the original license/notice files and an exact four-part file version.
Installs next to the selected Zellij executable (Cargo or extracted release).
Never replaces system files, changes environment variables, or overwrites an
existing provider directory. Run without elevation in a user-owned directory.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$SourceDirectory,
    [Parameter(Mandatory)][version]$ExpectedVersion,
    [Parameter(Mandatory)][string[]]$LicenseFiles,
    [Parameter(Mandatory)][uri]$SourceUrl,
    [string]$ZellijExecutable = $(if ($env:CARGO_HOME) { Join-Path $env:CARGO_HOME 'bin/zellij.exe' } else { Join-Path $HOME '.cargo/bin/zellij.exe' })
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows) { throw 'Requires PowerShell 7 on Windows.' }
if ($ExpectedVersion.Revision -lt 0 -or $ExpectedVersion -eq [version]'0.0.0.0') {
    throw 'ExpectedVersion must be a nonzero, four-part file version.'
}
if (-not $SourceUrl.IsAbsoluteUri -or $SourceUrl.Scheme -ne 'https') {
    throw 'SourceUrl must record the original HTTPS package/release URL; nothing is downloaded.'
}

function Assert-NoReparseChain([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    while ($null -ne $item) {
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Refusing reparse point: $($item.FullName)"
        }
        $parent = Split-Path -Parent $item.FullName
        if (-not $parent -or $parent -eq $item.FullName) { break }
        $item = Get-Item -LiteralPath $parent -Force
    }
}
function Get-Machine([string]$Path) {
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw "Not a PE file: $Path" }
        $reader.BaseStream.Position = 60
        $offset = $reader.ReadUInt32()
        $reader.BaseStream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550) { throw "Invalid PE signature: $Path" }
        return $reader.ReadUInt16()
    } finally { $reader.Dispose() }
}
function Get-VerifiedBinary([string]$Path, [int]$Machine) {
    if ((Get-Machine $Path) -ne $Machine) { throw "Architecture mismatch: $Path" }
    $info = [Diagnostics.FileVersionInfo]::GetVersionInfo($Path)
    $version = [version]::new($info.FileMajorPart, $info.FileMinorPart, $info.FileBuildPart, $info.FilePrivatePart)
    if ($version -ne $ExpectedVersion) { throw "Version mismatch: $Path is $version; expected $ExpectedVersion" }
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne 'Valid' -or $null -eq $signature.SignerCertificate -or
        $signature.SignerCertificate.GetNameInfo([Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false) -cne 'Microsoft Corporation') {
        throw "Not a valid Microsoft-signed binary: $Path ($($signature.Status))"
    }
    [ordered]@{
        name = [IO.Path]::GetFileName($Path)
        version = $version.ToString()
        sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
        signer = $signature.SignerCertificate.Subject
        signerThumbprint = $signature.SignerCertificate.Thumbprint
    }
}

$exe = (Resolve-Path -LiteralPath $ZellijExecutable).ProviderPath
Assert-NoReparseChain $exe
$machine = Get-Machine $exe
$arch = switch ($machine) {
    0x8664 { 'x86_64' }
    0xaa64 { 'aarch64' }
    0x14c { 'x86' }
    default { throw "Unsupported Zellij PE machine: $machine" }
}
$source = (Resolve-Path -LiteralPath $SourceDirectory).ProviderPath
Assert-NoReparseChain $source
foreach ($name in 'conpty.dll', 'OpenConsole.exe') {
    $path = Join-Path $source $name
    Assert-NoReparseChain $path
    $null = Get-VerifiedBinary $path $machine
}
if ($LicenseFiles.Count -eq 0) { throw 'Supply the original license and applicable notice files.' }
$licenses = @()
$licenseNames = @{}
foreach ($file in $LicenseFiles) {
    $path = (Resolve-Path -LiteralPath $file).ProviderPath
    Assert-NoReparseChain $path
    $item = Get-Item -LiteralPath $path
    if ($item.PSIsContainer -or $item.Length -eq 0) { throw "Empty/non-file license: $path" }
    if ($licenseNames.ContainsKey($item.Name)) { throw "Duplicate license filename: $($item.Name)" }
    $licenseNames[$item.Name] = $true
    $licenses += $path
}

$root = Join-Path (Split-Path -Parent $exe) 'conpty'
$destination = Join-Path $root $arch
if (Test-Path -LiteralPath $root) { Assert-NoReparseChain $root }
if (Test-Path -LiteralPath $destination) {
    throw "Provider directory already exists: $destination. Stop servers and move it aside before upgrading."
}
$null = New-Item -ItemType Directory -Path $root -Force
$stage = Join-Path $root ('.install-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $stage
try {
    $binaries = @()
    foreach ($name in 'conpty.dll', 'OpenConsole.exe') {
        $path = Join-Path $stage $name
        Copy-Item -LiteralPath (Join-Path $source $name) -Destination $path
        # Validate the copied bytes too, not just the source before copying.
        $binaries += Get-VerifiedBinary $path $machine
    }
    $licenseDir = Join-Path $stage 'licenses'
    $null = New-Item -ItemType Directory -Path $licenseDir
    $licenseRecords = @()
    foreach ($path in $licenses) {
        $target = Join-Path $licenseDir ([IO.Path]::GetFileName($path))
        Copy-Item -LiteralPath $path -Destination $target
        $licenseRecords += [ordered]@{
            name = [IO.Path]::GetFileName($target)
            sha256 = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
        }
    }
    [ordered]@{
        schema = 1; architecture = $arch; fileVersion = $ExpectedVersion.ToString()
        sourceUrl = $SourceUrl.AbsoluteUri; installedUtc = [DateTime]::UtcNow.ToString('o')
        binaries = $binaries; licenses = $licenseRecords
    } | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $stage 'manifest.json') -Encoding utf8NoBOM
    # Same-volume rename; Directory.Move refuses an existing destination.
    [IO.Directory]::Move($stage, $destination)
} finally {
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
}
Write-Host "Installed optional ConPTY $ExpectedVersion ($arch): $destination"
Write-Host 'Set conpty_provider "bundled" in config.kdl and start a NEW Zellij session to opt in.'
Write-Host 'Rollback: set conpty_provider "system" (and remove diagnostic environment overrides). Existing sessions are unchanged.'
[pscustomobject]@{ Directory = $destination; Version = $ExpectedVersion.ToString(); Architecture = $arch }
