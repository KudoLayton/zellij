# Optional Windows ConPTY provider

The system ConPTY remains the default. An explicitly selected, side-by-side
Microsoft `conpty.dll` + `OpenConsole.exe` pair can preserve SIXEL and terminal
queries that the tested inbox implementation drops. This is **not** a guarantee
of end-to-end image rendering. See [diagnostic evidence](../windows-graphics-diagnostic.md).
No Yazi change, system-file replacement, automatic download, or Windows Terminal
installation discovery is involved.

**Tested scope:** the `1.22.2507.30001` pair passed live preview,
clear/repaint, split, resize, and font enlargement/restoration checks in
Windows Terminal 1.24.12741.0 on x86_64, with the included SIXEL partial-band
padding fix. The original font-change failure and its regression fix are
recorded in the diagnostic evidence. Other versions/architectures remain
unverified, so treat this provider option as experimental. That host reports
10x20 **virtual SIXEL pixels** per cell even after font changes; this is not a
physical font-size measurement.

## Obtain and install

1. Obtain an architecture-matched pair from a trusted Microsoft ConPTY package
   or release, retaining its original license and all applicable notices.
   Consult [Microsoft Terminal](https://github.com/microsoft/terminal) and its
   [releases](https://github.com/microsoft/terminal/releases). Not every release
   archive necessarily contains a redistributable pair. Verify the actual
   package contents and redistribution terms; do not mix files from releases.
2. Build/install this Zellij revision, or extract a release containing it.
   `cargo install --path . --locked` installs the executable only, **not** the
   optional provider. Debug builds also need current wasm plugins; follow
   [CONTRIBUTING.md](../CONTRIBUTING.md) for the plugin build prerequisites.
   Do not replace a running executable or active provider.
3. Run PowerShell 7 without elevation in a user-owned installation directory.
   Supply the exact **file version**, not the package version, original source
   URL, and the license/notice files from that package:

   ```powershell
   # From this repository; replace these with your verified package details.
   $source = 'C:\trusted\conpty-package\x64'
   $version = '1.22.2507.30001' # tested specimen only; use YOUR pair's version
   $packageUrl = 'https://example.invalid/replace-with-original-package-url'
   ./scripts/install-conpty.ps1 -SourceDirectory $source `
       -ExpectedVersion $version -SourceUrl $packageUrl `
       -LicenseFiles 'C:\trusted\conpty-package\LICENSE','C:\trusted\conpty-package\NOTICE.md'
   ```

   Default executable: `$env:CARGO_HOME/bin/zellij.exe`, otherwise
   `$HOME/.cargo/bin/zellij.exe`. For an extracted release or local build add
   `-ZellijExecutable 'D:\apps\zellij\zellij.exe'`.

The installer checks both PE architectures against the selected executable,
exact matching four-part versions, and valid Microsoft Authenticode signatures.
It rejects missing binaries, reparse-point paths, empty/missing license files,
and an existing destination. License authenticity/completeness and redistribution
rights remain the packager's responsibility; nonempty files are not legal proof.

Files are copied into a private staging directory, revalidated, then renamed
without overwriting. Result:

```text
<directory containing zellij.exe>/conpty/x86_64/
  conpty.dll
  OpenConsole.exe
  licenses/<original license and notice files>
  manifest.json
```

`aarch64` and `x86` use their corresponding Rust architecture names. Only x86_64
has been exercised here. The manifest records versions, SHA-256 hashes,
Microsoft signer information, source URL, and license hashes. Keep it with any
redistributed pair. The helper performs no download and does not select a
"latest" version. Authenticode trust validation may use Windows certificate
services/network policy; validation failure is a hard error.

## Select, diagnose, and roll back

Set the following top-level option in your `config.kdl`, then start a **new
Zellij session**:

```kdl
conpty_provider "bundled"
```

Accepted values are `"system"` and `"bundled"`; omission defaults to `"system"`.
Other strings or non-string values are configuration errors. The option is
accepted but ignored on non-Windows hosts. It is a startup-only setting, not a
live setting in the configuration UI. The CLI equivalent is
`zellij options --conpty-provider bundled`.

The first client's resolved configuration selects one provider shared by all
panes in that server. Configuration reloads and attaching clients do not change
it. Use a new session name to apply a change; even new panes in an existing
session keep its original provider. The actual `zellij.exe` must be from this
revision; merely installing the DLLs next to an older executable has no effect.

For diagnostics or immediate rollback, the parent process environment variable
`ZELLIJ_CONPTY_PROVIDER` overrides the KDL/CLI value, including an explicit
`system`. Remove an old override if you want the config file to decide:

```powershell
Remove-Item Env:ZELLIJ_CONPTY_PROVIDER -ErrorAction SilentlyContinue
zellij --session graphics-test
```

The installer does not set environment variables or modify configuration.
Setting an override inside an existing pane is too late.

Optional explicit directory (also useful for `cargo test`, whose executable
lives under `target/debug/deps`):

```powershell
$env:ZELLIJ_CONPTY_DIR = 'D:\apps\zellij\conpty\x86_64'
```

This must be an **absolute, trusted directory** containing both matching files.
A directory setting alone never opts into DLL loading. Do not use a shared or
untrusted writable directory. Runtime loading checks architecture and matching
nonzero versions, resolves the three required `Conpty*PseudoConsole` exports,
and restricts dependency lookup to the DLL's directory and System32; it does
not search PATH or the current directory. Runtime does **not** revalidate the
installer manifest or Authenticode signature: protect installed files with
normal filesystem permissions. No defense against an attacker who can rewrite
your executable/provider directory is implied.

Create, resize, and close use the same provider; it stays loaded until the last
owned pseudo-console is closed. Successful creation logs the provider path,
file version, and effective flags. Loading/validation failures log a warning
and use system ConPTY; **that fallback does not imply graphics support**.
An error after successful loading during pseudo-console creation remains a
spawn error, rather than mixing providers mid-lifecycle.

The bundled provider uses standard flags `0`. The system provider retains its
existing flag fallback policy and `ZELLIJ_CONPTY_NO_PASSTHROUGH` compatibility
setting. Its historical bit-8 name is misleading: current Microsoft headers
name bit 8 a glyph-width mode, not a graphics bypass.

Rollback for new servers: set `conpty_provider "system"` in `config.kdl`
(or remove the option to restore the default), and remove any diagnostic
provider override. Alternatively, force system mode in the parent shell:

```powershell
$env:ZELLIJ_CONPTY_PROVIDER = 'system'
Remove-Item Env:ZELLIJ_CONPTY_DIR -ErrorAction SilentlyContinue
```

Removing the environment variable alone does **not** roll back an explicit
`conpty_provider "bundled"` configuration.

Existing sessions remain untouched. To upgrade/remove files, first save work
and stop the servers that use them, then move the old architecture directory
aside and rerun the installer. Never overwrite loaded DLLs or system files.

## Verification

Installer integration checks use temporary Cargo-style and release-style
locations, leave the real installation untouched, and require the same trusted
package inputs:

```powershell
./scripts/test-install-conpty.ps1 -SourceDirectory $source `
    -ExpectedVersion $version -SourceUrl $packageUrl `
    -LicenseFiles 'C:\trusted\conpty-package\LICENSE','C:\trusted\conpty-package\NOTICE.md' `
    -ZellijExecutable "$HOME/.cargo/bin/zellij.exe"
```

Backend regression and opt-in graphics tests (from this repository):

```powershell
$env:ZELLIJ_CONPTY_PROVIDER = 'bundled' # repeat with 'system'
$env:ZELLIJ_CONPTY_DIR = 'D:\apps\zellij\conpty\x86_64'
$env:ZELLIJ_TEST_CONPTY_EXPECT = $env:ZELLIJ_CONPTY_PROVIDER
cargo test -p zellij-server --lib os_input_output_windows -- --test-threads=1
$env:ZELLIJ_DIAG_YA = 'D:\src\yazi\target\debug\ya.exe' # freshly built current main
cargo test -p zellij-server --lib graphics_diagnostics -- --ignored --nocapture --test-threads=1
cargo test -p zellij-server --lib bundled_validation_failures_fall_back -- --ignored --test-threads=1
cargo test -p zellij-server --lib pwsh_unicode_input_survives_split_utf8 -- --ignored --test-threads=1
```

The test-only expectation detects unintended fallback. Graphics diagnostics
use controlled host responses, not a live Zellij grid: synthetic 10x20 pixels
must not be reported as the terminal's measured cell size. Still verify on a
real attached terminal: `ya env` outside/inside Zellij, actual image preview,
clear/repaint, pane splits, resize, and font-size changes. Keep those checks
open until observed; an inherited `WT_SESSION` or `Drivers.matches: Sixel`
alone is not evidence that graphics work.
