#requires -Version 7.0
[CmdletBinding()]
param(
    [ValidateSet('cmd', 'winps-cooked', 'pwsh')][string]$Shell = 'pwsh',
    [ValidateSet('wt', 'native')][string]$Terminal = 'wt',
    [string]$Binary = "$HOME/.cargo/bin/zellij.exe",
    [switch]$PrepareOnly
)

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Run this helper in PowerShell 7 on Windows.' }
$Binary = (Resolve-Path -LiteralPath $Binary).Path
$session = 'zw-ui-' + [guid]::NewGuid().ToString('N').Substring(0, 12)
$root = Join-Path ([IO.Path]::GetTempPath()) $session
$null = New-Item -ItemType Directory $root
$config = Join-Path $root 'config.kdl'
$layout = Join-Path $root 'layout.kdl'

function KdlString([string]$Value) {
    '"' + $Value.Replace('\', '\\').Replace('"', '\"') + '"'
}

switch ($Shell) {
    'cmd' { $program = $env:ComSpec; $shellArgs = @('/d') }
    'winps-cooked' {
        $program = "$env:SystemRoot/System32/WindowsPowerShell/v1.0/powershell.exe"
        $shellArgs = @('-NoLogo', '-NoProfile', '-NoExit', '-Command',
            'Remove-Module PSReadLine -ErrorAction SilentlyContinue')
    }
    'pwsh' {
        # Use this host's executable, not a potentially multi-match PATH lookup.
        $program = Join-Path $PSHOME 'pwsh.exe'
        $shellArgs = @('-NoLogo', '-NoProfile', '-NoExit', '-Command',
            'Set-PSReadLineOption -HistorySaveStyle SaveNothing')
    }
}
@'
show_startup_tips false
show_release_notes false
keybinds clear-defaults=true {
    normal {
        bind "Ctrl g" "Alt l" { SwitchToMode "Locked"; }
        bind "Ctrl d" { Detach; }
        bind "Ctrl q" { Quit; }
    }
    locked {
        bind "Ctrl g" "Alt l" { SwitchToMode "Normal"; }
    }
}
'@ | Set-Content -Encoding utf8NoBOM $config
$quotedArgs = ($shellArgs | ForEach-Object { KdlString $_ }) -join ' '
@"
layout {
    pane size=1 borderless=true { plugin location="zellij:tab-bar"; }
    pane command=$(KdlString $program) cwd=$(KdlString $root) {
        args $quotedArgs
    }
    pane size=2 borderless=true { plugin location="zellij:status-bar"; }
}
"@ | Set-Content -Encoding utf8NoBOM $layout
[IO.File]::WriteAllText((Join-Path $root 'paste.txt'), ('paste0123456789-' * 256) + '한글😀끝')
@'
param([switch]$ForCmd, [switch]$PrintOnly)
$text = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'paste.txt'))
if ($ForCmd) {
    if ($text -match '[\r\n&|<>^%!\"]') { throw 'Unsafe cmd fixture contents' }
    $text = "echo $text>pasted.txt"
}
if ($PrintOnly) { $text } else { Set-Clipboard -Value $text }
'@ | Set-Content -Encoding utf8NoBOM (Join-Path $root 'copy-paste.ps1')
@'
$expected = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'paste.txt')) + "`r`n"
$actual = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'pasted.txt'))
$ok = $actual -ceq $expected
Write-Output $ok
if (-not $ok) {
    Write-Output "Expected UTF-16 length $($expected.Length), actual $($actual.Length)"
    exit 1
}
'@ | Set-Content -Encoding utf8NoBOM (Join-Path $root 'check-paste.ps1')
[ordered]@{
    session = $session; terminal = $Terminal; shell = $Shell
    binary = $Binary; sha256 = (Get-FileHash $Binary -Algorithm SHA256).Hash
    created = (Get-Date).ToString('o'); visualVerification = 'PENDING user observation'
} | ConvertTo-Json | Set-Content -Encoding utf8NoBOM (Join-Path $root 'case.json')

Write-Host "Case: $Terminal / $Shell; records: $root"
if ($PrepareOnly) {
    & $Binary --config $config --config-dir $root setup --check
    if ($LASTEXITCODE) { throw "Configuration check failed: $LASTEXITCODE" }
    Write-Host 'Preparation only: no terminal UI was exercised.'
    return
}

Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class ZellijManualConsole {
    [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int which);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetConsoleMode(IntPtr h, out uint mode);
    [DllImport("kernel32.dll")] public static extern uint GetConsoleCP();
    [DllImport("kernel32.dll")] public static extern uint GetConsoleOutputCP();
    public static uint Mode(int which) {
        uint value;
        if (!GetConsoleMode(GetStdHandle(which), out value)) throw new Win32Exception();
        return value;
    }
}
'@
function ConsoleState {
    [ordered]@{
        inputMode = [ZellijManualConsole]::Mode(-10)
        outputMode = [ZellijManualConsole]::Mode(-11)
        inputCodePage = [ZellijManualConsole]::GetConsoleCP()
        outputCodePage = [ZellijManualConsole]::GetConsoleOutputCP()
    }
}
function RecordRestore([string]$Stage, $Before, [int]$ExitCode) {
    $after = ConsoleState
    $restored = (($Before | ConvertTo-Json -Compress) -ceq ($after | ConvertTo-Json -Compress))
    [ordered]@{ stage = $Stage; exitCode = $ExitCode; restored = $restored; before = $Before; after = $after } |
        ConvertTo-Json -Depth 5 | Set-Content -Encoding utf8NoBOM (Join-Path $root "$Stage.json")
    Write-Host "$Stage : exit=$ExitCode console modes/code pages restored=$restored"
}

# The helper must be run in a separate pwsh -NoProfile process, not dot-sourced.
# Only this child's environment and the unique test session are changed.
$names = @('ZELLIJ', 'ZELLIJ_SESSION_NAME', 'ZELLIJ_PANE_ID', 'ZELLIJ_SOCKET_DIR',
    'ZELLIJ_CONFIG_FILE', 'ZELLIJ_CONFIG_DIR', 'ZELLIJ_LAYOUT_DIR')
# A conhost launched from WT inherits WT_SESSION. Clear emulator markers in
# this child only, so the native case actually exercises INPUT_RECORD input.
if ($Terminal -eq 'native') { $names += @('TERM', 'WT_SESSION') }
$saved = @{}
foreach ($name in $names) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
try {
    foreach ($name in $names) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
    # Keep the normal socket namespace paired with its shared session index.
    # A separate socket namespace with the normal cache can lose session aliases.
    Write-Host 'Verify IME, emoji display, paste, resize; Ctrl+g and Alt+l toggle Locked/Normal.'
    Write-Host 'First leave with Ctrl+d (Normal mode). Then reattach and leave with Ctrl+q.'
    $null = Read-Host 'Press Enter to begin'
    $before = ConsoleState
    & $Binary --config $config --config-dir $root --session $session --new-session-with-layout $layout
    RecordRestore 'detach' $before $LASTEXITCODE
    $null = Read-Host 'Verify cursor, echo and prompt restoration. Press Enter to reattach'
    $before = ConsoleState
    & $Binary --config $config --config-dir $root attach $session
    RecordRestore 'quit' $before $LASTEXITCODE
}
finally {
    # Never kill/delete other sessions. Keep evidence files for review.
    & $Binary --config $config --config-dir $root delete-session --force $session 2>$null
    foreach ($name in $names) {
        if ($null -eq $saved[$name]) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
    }
    Write-Host "Evidence retained: $root"
}
$null = Read-Host 'Check cursor/echo/alternate-screen restoration; report observations, then Enter to close'
