#requires -Version 7.0
# Opt-in integration test; no downloads and no changes to the real installation.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$SourceDirectory,
    [Parameter(Mandatory)][version]$ExpectedVersion,
    [Parameter(Mandatory)][string[]]$LicenseFiles,
    [Parameter(Mandatory)][uri]$SourceUrl,
    [Parameter(Mandatory)][string]$ZellijExecutable
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$installer = Join-Path $PSScriptRoot 'install-conpty.ps1'
$root = Join-Path ([IO.Path]::GetTempPath()) ('zellij-conpty-install-test-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $root
$common = @{
    SourceDirectory = (Resolve-Path -LiteralPath $SourceDirectory).ProviderPath
    ExpectedVersion = $ExpectedVersion; LicenseFiles = $LicenseFiles; SourceUrl = $SourceUrl
}
function Expect-Rejection([string]$Name, [hashtable]$Arguments, [string]$Pattern) {
    $rejected = $false
    try { & $installer @Arguments | Out-Null }
    catch {
        $errorText = $_.ToString() + ' ' + $_.FullyQualifiedErrorId
        if ($errorText -notmatch $Pattern) { throw "Wrong rejection for ${Name}: $_" }
        $rejected = $true
    }
    if (-not $rejected) { throw "Expected rejection: $Name" }
    Write-Host "PASS rejected: $Name"
}
try {
    # Both supported deployment layouts use the executable's own directory.
    foreach ($layout in @('.cargo/bin', 'release')) {
        $bin = Join-Path $root $layout
        $null = New-Item -ItemType Directory -Path $bin -Force
        $exe = Join-Path $bin 'zellij.exe'
        Copy-Item -LiteralPath $ZellijExecutable -Destination $exe
        $args = $common.Clone(); $args.ZellijExecutable = $exe
        $result = & $installer @args
        $manifestPath = Join-Path $result.Directory 'manifest.json'
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        if ($manifest.fileVersion -ne $ExpectedVersion.ToString() -or $manifest.binaries.Count -ne 2) { throw 'Bad manifest' }
        foreach ($record in $manifest.binaries) {
            if ((Get-FileHash -LiteralPath (Join-Path $result.Directory $record.name)).Hash -ne $record.sha256) { throw 'Hash mismatch' }
        }
        if ($manifest.licenses.Count -ne $LicenseFiles.Count) { throw 'Missing licenses' }
        $before = (Get-FileHash -LiteralPath $manifestPath).Hash
        Expect-Rejection "overwrite $layout" $args 'already exists'
        if ((Get-FileHash -LiteralPath $manifestPath).Hash -ne $before) { throw 'Existing install modified' }
        Write-Host "PASS installed: $layout"
    }
    $bin = Join-Path $root 'rejections'
    $null = New-Item -ItemType Directory -Path $bin
    $exe = Join-Path $bin 'zellij.exe'
    Copy-Item -LiteralPath $ZellijExecutable -Destination $exe
    $args = $common.Clone(); $args.ZellijExecutable = $exe
    $bad = $args.Clone(); $bad.ExpectedVersion = [version]'65535.65535.65535.65535'
    Expect-Rejection 'wrong pinned version' $bad 'Version mismatch'
    $bad = $args.Clone(); $bad.LicenseFiles = @((Join-Path $root 'missing-license'))
    Expect-Rejection 'missing license' $bad 'PathNotFound|ItemNotFound'
    $empty = Join-Path $root 'empty-license'
    [IO.File]::WriteAllText($empty, '')
    $bad = $args.Clone(); $bad.LicenseFiles = @($empty)
    Expect-Rejection 'empty license' $bad 'Empty/non-file'
    $bad = $args.Clone(); $bad.SourceUrl = [uri]'http://example.invalid/not-trusted'
    Expect-Rejection 'insecure provenance URL' $bad 'HTTPS'

    $pair = Join-Path $root 'bad-pair'
    $null = New-Item -ItemType Directory -Path $pair
    Copy-Item -LiteralPath (Join-Path $SourceDirectory 'conpty.dll') -Destination $pair
    $bad = $args.Clone(); $bad.SourceDirectory = $pair
    Expect-Rejection 'missing host' $bad 'PathNotFound|ItemNotFound'
    Copy-Item -LiteralPath "$env:SystemRoot/System32/kernel32.dll" -Destination (Join-Path $pair 'OpenConsole.exe')
    Expect-Rejection 'mismatched pair' $bad 'Version mismatch|Architecture mismatch'
    Copy-Item -LiteralPath (Join-Path $SourceDirectory 'OpenConsole.exe') -Destination $pair -Force
    $dll = Join-Path $pair 'conpty.dll'
    $original = [IO.File]::ReadAllBytes($dll)
    $bytes = $original.Clone()
    $pe = [BitConverter]::ToUInt32($bytes, 60)
    $bytes[$pe + 4] = 0; $bytes[$pe + 5] = 0
    [IO.File]::WriteAllBytes($dll, $bytes)
    Expect-Rejection 'wrong architecture' $bad 'Architecture mismatch'
    $bytes = $original.Clone()
    $bytes[0x200] = $bytes[0x200] -bxor 1
    [IO.File]::WriteAllBytes($dll, $bytes)
    Expect-Rejection 'tampered signature' $bad 'Microsoft-signed'
    if (Test-Path -LiteralPath (Join-Path $bin 'conpty')) { throw 'Rejected inputs created an installation' }
    Write-Host 'PASS all installer checks (two layouts, overwrite protection, eight invalid inputs)'
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force
}
