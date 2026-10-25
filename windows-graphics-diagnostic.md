# Windows graphics diagnostic — current main

## Scope

- Zellij baseline: `4d3519267` (KudoLayton main).
- Yazi: `40a91421`, locally built `ya.exe` and `yazi.exe` (26.9.1).
- Windows: `10.0.26300.0`.
- PATH Yazi/Ya were older `aa52643`; these were not used for the main-source probe.
- Initial diagnosis left production code unchanged. The subsequent optional-provider implementation is recorded below; the system provider remains the default. Opt-in tests use the actual `WindowsPtyBackend` and real Windows ConPTY. They do not launch the Zellij client/grid or verify a physical terminal display.

## Transport observations

The child enables VT output, uses Win32 `WriteFile`, checks successful full writes, and emits a small SIXEL between text markers. The test drains the real backend output to EOF and checks normal child exit and marker delivery.

| Observation | Actual flags 14 | Actual flags 6 |
| --- | --- | --- |
| Text before/after SIXEL received | yes | yes |
| Exact SIXEL received | no | no |
| SIXEL DCS/payload visible in captured output | no | no |
| DA1 (`CSI c`) forwarded | no | no |
| XTVERSION (`CSI > q`) forwarded | yes | yes |
| Cell-size query (`CSI 16 t`) forwarded | yes | yes |

The captured graphics region is `GRAPHICS_BEGINGRAPHICS_END`. This reproduces a transport blocker before Zellij's grid parses the stream; it does not merely infer failure from Yazi's driver choice. Disabling the locally named passthrough flag does not fix it.

## Current-main `ya env` observations

The test runs the freshly built `ya env` in the real backend. Output is redirected to a temporary file to avoid ConPTY line-wrapping corruption. In controlled-response cases the harness replies to forwarded XTVERSION with `Zellij 0.46.0`, and to `CSI 16 t` with a 10x20 cell. DA1 would receive a SIXEL-capable response if forwarded, but it is not observed.

| Host response simulation | Brand environment | Detected brand | Driver | sixel capability | csi_16t |
| --- | --- | --- | --- | --- | --- |
| none | inherited WT_SESSION | Microsoft | Sixel | false | (0, 0) |
| enabled | inherited WT_SESSION | Microsoft | Sixel | false | (0, 0) |
| enabled | terminal-brand hints removed | Unknown | Chafa | false | (0, 0) |

In all cases `TERM.dimension` reports 80x24 cells and zero pixel dimensions. Even when the harness sends the forwarded-query replies, this standalone probe's report does not incorporate them. This does not establish what the running Yazi TUI's later event processing would observe. The provider comparison below subsequently confirmed an intervening local DA1 response and recovered the probe results by replacing the ConPTY provider.

`Drivers.matches: Sixel` is not proof of successful SIXEL negotiation: Yazi selects SIXEL unconditionally for the Microsoft brand inherited through `WT_SESSION`.

## Interpretation and remaining checks

1. A real SIXEL transport failure is reproduced on this OS/backend before the Zellij image parser.
2. The flag named `PASSTHROUGH = 8` in the local code is not a demonstrated transport switch. Microsoft's current `winconpty.h` defines `0x08` as `PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES` (https://github.com/microsoft/terminal/blob/main/src/winconpty/winconpty.h).
3. The attached-terminal checks and follow-up fix below verify live detection, image display, and font-change redraw. Windows Terminal deliberately reports virtual SIXEL pixels, not physical font pixels. The synthetic 10x20 reply in the backend tests must still be distinguished from the live host response.
4. For the system provider these tests are observation tools: a pass means capture/probe execution succeeded, **not** that graphics work. The subsequent bundled-provider tests additionally assert exact SIXEL/query forwarding and controlled Yazi detection; they still do not verify the display.

## Provider A/B: confirmed transport and detection difference

An isolated Win32 harness uses the same pipes, `CreateProcessW` pseudo-console attribute, child command, query replies, and output capture. Only the provider changes:

- Inbox: `kernel32!CreatePseudoConsole` / `ClosePseudoConsole`; system `conhost.exe` file version `10.0.26100.8875`.
- Side-by-side: `conpty.dll!ConptyCreatePseudoConsole` / `ConptyClosePseudoConsole` plus its matching `OpenConsole.exe`; both file version `1.22.2507.30001`, both valid Microsoft signatures. This version is the tested specimen, not a claim about the minimum or recommended release.

| Provider | Flags | Text markers | Exact SIXEL | DA1 forwarded | XTVERSION / cell query forwarded |
| --- | --- | --- | --- | --- | --- |
| inbox | 0 | yes | no | no | yes / yes |
| inbox | 14 | yes | no | no | yes / yes |
| side-by-side | 0 | yes | yes | yes | yes / yes |
| side-by-side | 14 | yes | yes | yes | yes / yes |

All children exited successfully. The input probe explicitly enables VT input/output and requests XTVERSION, cell pixels, then DA1. With the same controlled host replies, the child receives:

```text
inbox:       ESC[?61;6;7;21;22;23;24;28;32;42c
side-by-side: ESCP>|Zellij 0.46.0 ESC\  ESC[6;20;10t  ESC[?62;4;52c
```

The spaces above are for readability; the side-by-side capture is 40 bytes with no inserted separators. The inbox capture is a 31-byte local DA1 response without SIXEL attribute 4. Yazi's `yazi-emulator/src/probe.rs` returns on the first DA1 when no passthrough restart is needed; this explains why a locally generated response can end the probe before the forwarded host replies are incorporated.

Running the **same current-main `ya.exe env`**, inherited brand environment, and controlled responses with flags 0 gives:

| Provider | Brand.from_env | Emulator brand | SIXEL capability | csi_16t | Driver |
| --- | --- | --- | --- | --- | --- |
| inbox | Microsoft | Microsoft | false | (0, 0) | Sixel |
| side-by-side | Microsoft | Zellij | true | (10, 20) | Sixel |

No Yazi production changes were needed. The 10x20 value is synthetic, not the real terminal's font measurement. `TERM.dimension` still has zero pixel dimensions in both cases, so those zeros alone do not establish failure: the separately probed `csi_16t` is important.

The responder handles both `CSI c` and Yazi's `CSI 0 c`, and every repeated query, including the side-by-side provider's startup DA1. The Rust diagnostic was corrected to do the same and its two opt-in tests were rerun successfully.

Temporary evidence is under `%TEMP%\zellij-conpty-provider-diag`: `ProviderProbe.cs`, `transport-results.json`, `input-{kernel32,conpty}.txt`, and `ya-{kernel32,conpty}.txt`. These are local research artifacts, not a shipped backend or permanent repository test fixture.

## Fix rationale, recorded before implementation

1. **Use a tested side-by-side ConPTY provider in Zellij.** In `zellij-server/src/windows_conpty.rs`, introduce a provider that owns the selected create/resize/close function table and library lifetime. Load a pinned, architecture-matched `conpty.dll` and `OpenConsole.exe` pair from a trusted installation path. Use an absolute DLL path and a restricted dependency search; never load an arbitrary DLL from the current directory. Merely copying the DLL next to Zellij does not replace its current static system imports.
2. **Keep the entire HPCON lifecycle on that provider.** Route resize in `zellij-server/src/os_input_output_windows.rs` through the same provider as allocation and close. Keep the library alive until all consoles are closed. Preserve existing output-draining, input cancellation, child-exit, and close-order guarantees. Package the matching binaries and licenses; cover both release packaging and development installation. Do not replace system files or discover an arbitrary Windows Terminal installation as a production dependency.
3. **Correct the flag policy and expose diagnostics.** `PASSTHROUGH = 8` is not a demonstrated graphics bypass; Microsoft's current header calls that bit a glyph-width mode. Start from supported flags for the chosen provider (flags 0 already passed this transport probe), and test any required cursor/width behavior separately. Log provider path/version and effective flags. If bundled-provider loading fails, preserve an explicit text-only/system fallback with a warning rather than implying graphics capability. Revisit the misleading `ZELLIJ_CONPTY_NO_PASSTHROUGH` setting and its tests.
4. **Validate the real Zellij reply path before changing Yazi.** The A/B responder is not Zellij's grid. `panes/grid.rs` currently forwards `CSI 16 t` asynchronously while other replies can be local. Verify response ordering against Yazi's first-DA1 completion rule. If necessary, answer cell-size queries from valid cached cell metrics, or coordinate completion of preceding forwarded queries; do not hard-code the test's 10x20 size. Check resize/font-change invalidation. Treat this ordering concern as a remaining integration risk, not a second experimentally proven failure in the live UI.
5. **Do not start with Yazi's SIXEL encoder.** Provider replacement already restored transport and current Yazi detection in isolation. A separate Yazi robustness change could avoid treating inherited `WT_SESSION` as sufficient SIXEL evidence inside an unsupported mux, but it cannot restore bytes discarded by ConPTY. Make such a change only after the provider and real response path are verified.

### Acceptance checks for implementation

- Run transport assertions under both providers: exact SIXEL preservation on the supported provider; document rather than conceal the tested inbox limitation.
- Run current-main `ya env` directly in Windows Terminal and in a real Zellij pane. Confirm the pane reports Zellij, SIXEL capability, and nonzero **actual host-reported** cell pixels; do not accept `Drivers.matches: Sixel` alone. These are virtual SIXEL units on Windows Terminal, as established in the follow-up below, not physical font measurements.
- Verify an actual image preview, clearing/repainting, pane split, terminal/pane resize, and font-size change. Capture the final display; byte forwarding alone is not proof of image rendering.
- Re-run Windows backend tests for spawn, resize, input, natural exit, explicit close, backpressure, and multiple panes. Check missing/mismatched DLL fallback and unsupported architecture behavior.
- Leave real attached-terminal checks open until performed and passing. The initial font-change failure below required a separate grid regression fix before that check could pass.

## Optional-provider implementation and verification

Implemented in `windows_conpty_provider.rs`, with create/resize/close routed
through the same lifetime-owned provider. Loading is explicitly opt-in; system
behavior remains the default. Bundled mode uses flags 0. Installation,
provenance recording, trust assumptions, and rollback are documented in
[docs/windows-conpty.md](docs/windows-conpty.md).

Observed checks on x86_64 Windows, with the same Microsoft-signed
`1.22.2507.30001` pair and current-main Yazi `40a91421`:

| Check | Result |
| --- | --- |
| `os_input_output_windows` regression suite, system | 51 passed, 8 opt-in/helper tests ignored |
| Same suite, bundled, with provider expectation assertion | 51 passed, 8 ignored |
| PowerShell 7 split UTF-8/non-BMP input, explicitly enabled | Passed under both providers |
| SIXEL and query diagnostic, bundled | Exact SIXEL, DA1, XTVERSION, and cell query preserved; flags 0 |
| Controlled Yazi response, bundled, with/without inherited terminal brand | Assertions passed: Zellij, `sixel: true`, synthetic `csi_16t: (10, 20)` |
| Same diagnostics, system | SIXEL/DA1 still absent; Yazi SIXEL capability false, cell query result `(0, 0)` |
| Actual loader rejection/fallback fixtures | Missing host, mismatched version, wrong architecture, and missing exports all rejected; system fallback selected |
| Installer integration | Cargo/release temporary layouts passed; existing installs preserved; eight invalid inputs rejected |
| Default-feature `cargo build --locked --bin zellij` | Passed after building the required wasm plugins; `--version` reports 0.46.0 |
| Whitespace diff check | Passed |

The build initially hit command time limits while compiling vendored OpenSSL,
then exposed missing debug wasm assets. Both were resolved without disabling
default features or editing dependency/build policy. Existing unrelated compiler
warnings remain. This is not a whole-workspace/Linux test result.

Evidence logs are local `%TEMP%/zellij-conpty-{system,bundled}-{regression,graphics}.log`
and `%TEMP%/zellij-conpty-default-build.log`. No provider binaries were added to
the repository, and the installed user executable, global environment, running
sessions, and Yazi production source were not changed.

## Attached-terminal verification (2026-10-25)

With permission, opened a separate Windows Terminal **stable 1.24.12741.0**
window using the existing profile, PowerShell 7 without a profile, isolated
Yazi configuration, and temporary Zellij configuration/layouts. This was not
the user's existing Windows Terminal Preview window. Used the freshly built
`target/debug/zellij.exe`, current-main Yazi, and the same bundled pair as above.
The test shells recorded `InputRedirected=false` and `OutputRedirected=false`.
`ya env` reports were redirected to files, while terminal queries used the real
attached console; there was no synthetic responder.

| Execution path | Live emulator brand | SIXEL capability | Live-reported `csi_16t` | Image |
| --- | --- | --- | --- | --- |
| Direct Windows Terminal | Microsoft | true | (10, 20) | visible |
| Zellij, bundled provider | Zellij (`Zellij(4600)`) | true | (10, 20) | visible |
| Zellij, system provider | Microsoft | false | (0, 0) | not visible; fell back to unavailable `chafa` |

Both bundled pane creations logged the selected `conpty.dll`, version
`1.22.2507.30001`, and `effective_flags=0x0`. The live result establishes that
Zellij's reply path works for this baseline; it does not prove all query timing.
The system case demonstrates why inherited `WT_SESSION`/a Sixel driver name
must not be accepted as successful graphics detection.

Used a generated 320x240 PNG with four colored quadrants, an outlined circle,
and a label, plus a text file for the clearing test. Captured only the isolated
diagnostic window with `PrintWindow` and inspected the images.

| Initial UI check, before the follow-up fix | Observed result |
| --- | --- |
| Image preview | Visible, with all four quadrants and label |
| Select text, then image | Image cleared completely, then reappeared |
| Split pane downward | Image repainted within the upper pane |
| Increase upper pane height | Image and pane contents repainted |
| Resize window from 1200x850 to 1000x700 | Image moved/repainted; no old image remained |
| Increase font with three Ctrl+numpad-plus presses | **Failed**: image overlapped the status area; subsequent navigation left stale/duplicated text |
| Reset font with Ctrl+0 and restart preview | Clean preview recovered |

The font test really changed geometry: the bundled upper pane went from
105x16 to 79x13 cells, while the live `ya env` cell report remained (10,20).
A separate **direct-Windows-Terminal control** also visibly enlarged the
font, reported 79 columns, and still reported (10,20), but text/image clearing
and repainting remained clean. Thus the unchanged cell report is not evidence of a stale
Zellij-only cache; the additional redraw corruption inside Zellij was real,
but its ownership (client rendering, grid, nested ConPTY, or interaction with
Yazi) was not isolated in this first round. At that point the pair could not
be labeled fully validated. A returned nonzero value is not proof of the
physical font's pixel dimensions; do not hard-code a different size.

Local evidence: `%TEMP%/zellij-live-20261025/`, including
`direct-baseline.txt`, `bundled-initial.txt`, `system-initial.txt`,
`bundled-larger.txt`, `direct-larger.txt`, paired `*-initial.json` metadata,
and the `direct-*`/`bundled-*`/`system-image.png` screenshots. In particular,
compare `bundled-larger-clear.png` with `direct-larger-clear.png`.
`bundled-font-up.png` was an earlier no-op keybinding attempt and is **not**
font-change evidence; the successful keybinding produced `*-font-larger.png`.
Temporary scripts/layouts are retained beside the evidence. A new session with
a file layout requires `--new-session-with-layout`, not `--session ... --layout`
(which targets an existing session in this revision).

After this first round, both test sessions and all test windows/children were
closed. Process/session inspection at that point confirmed the pre-existing
`kudolayton` server/client and Preview window remained running; no diagnostic
session remained. No installed binary,
user configuration, global environment, or Yazi source was modified.

The font-change failure was left open at this point, then investigated with
additional approval as follows.

## Follow-up: partial SIXEL band fix

The failure is not a ConPTY resize failure or merely a host repaint artifact:
a read-only snapshot of the isolated child's ConPTY screen buffer retained the
header and correct status line, while Zellij's `dump-screen` already contained
the missing header and displaced text. Captured PTY bytes contained a valid
full repaint followed by a transparent SIXEL raster declared as 267x200.

`sixel-image` 0.2.1 allocates complete six-row bands. A 200px raster therefore
had 204 stored rows, with four transparent padding rows at its bottom. Zellij
used that padded height to advance the cursor: in a 12-row pane with 20px
virtual cells, drawing at row 2 incorrectly scrolled the header off by one row.
Later incremental updates then exposed stale text. A small grid regression
reproduced exactly that header/status displacement without Windows or Yazi.

Fix in `zellij-server/src/panes/sixel.rs`: retain the declared raster height
and transparency while parsing, and trim **only** transparent padding in the
final partial six-row band. Do not clip painted pixels beyond the declaration,
extra whole bands, opaque backgrounds, or images without a positive declared
height. Raster attributes are not a general clipping boundary. No ConPTY flags,
client resize policy, dependency version, or Yazi source was changed for this
follow-up.

Verification after the fix:

- The new resize/header regression failed before the change and passes after it.
- A second regression checks eight boundary cases and metadata reset between
  images, including real pixels extending beyond the declared height.
- SIXEL-focused suite: **31 passed**; pane suite: **729 passed**, one existing
  opt-in grapheme-throughput benchmark ignored. These suites overlap.
- Windows backend suite rerun under both providers: **51 passed, 8 opt-in/helper
  tests ignored each**.
- Default-feature executable rebuild, `sixel.rs` rustfmt check, and whitespace
  diff check passed. Existing unrelated warnings remain; no snapshots changed.
- Same live stable-WT window/profile, same `1.22.2507.30001` provider/flags 0,
  same image and split layout: font enlargement, clearing, repainting, live
  Ctrl+0 restoration, repeated enlargement, window resize, and pane size
  increase/decrease all passed. Header/status placement stayed intact without
  restarting Yazi. Confirmed with screenshots and grid dumps.

Evidence: `%TEMP%/zellij-font-20261025/`. Compare `after-font-clear.png` with
`fixed-font-clear-confirmed.png` and `fixed-cycle-clear.png`; see also
`fixed-font-repaint.png`, `fixed-cycle-repaint.png`, `fixed-reset-live.png`,
`fixed-pane-increase.png`, `fixed-font-clear-grid-confirmed.txt`, and
`fixed-conpty-buffer.txt`. PTY captures are in `vt-*.bin`/`delta-*.txt`; test
logs are `sixel-tests.log`, `panes-tests.log`, and
`{system,bundled}-regression-after-fix.log`. The initial attempted
`fixed-font-clear.png` did not navigate because the diagnostic IPC temp path
had changed; it is not clearing evidence. The confirmed captures used
session- and pane-targeted CLI key delivery instead.

### Why the live cell report remains (10,20)

This is intentional in the tested host, not a stale Zellij cache.
Windows Terminal's version-tagged source explicitly answers CSI 14t with
**virtual** text-area pixels and CSI 16t with `SixelParser::CellSizeForLevel()`:

- [AdaptDispatch::WindowManipulation, v1.24.12741.0](https://github.com/microsoft/terminal/blob/v1.24.12741.0/src/terminal/adapter/adaptDispatch.cpp)
- [SixelParser::CellSizeForLevel, same version](https://github.com/microsoft/terminal/blob/v1.24.12741.0/src/terminal/adapter/SixelParser.cpp)

The default VT240/VT340-compatible virtual cell is 10x20; it does not change
with the displayed font. Both direct and in-pane live reports agree with that
contract, including after enlargement (bundled pane: 79x12 cells). It would be
incorrect to replace this with guessed physical font dimensions. Physical
screen pixels were not measured or promised by these protocol reports.

All follow-up diagnostic sessions, windows, and child processes were closed;
the existing Preview window remains open. At final audit the earlier installed
`kudolayton` server/client were no longer present; the shared log records their
normal exit at 09:09:42, before the fixed-binary session began at 09:10:54.
The user confirmed that they intentionally closed that existing session.
Only specifically named `graphics-*` sessions were targeted by diagnostic
cleanup. No installed executable, user configuration, global environment,
or Yazi source was changed.

The tested live matrix now passes. Scope remains x86_64, this provider pair,
this Windows Terminal version/profile, and the exercised image/navigation
cases—not arbitrary terminals, architectures, images, or all workspace tests.

## KDL selection and pre-install verification (2026-10-25)

The follow-up implementation adds `conpty_provider "system" | "bundled"` to
KDL parsing/serialization, CLI option merging and the client/server protobuf
contract (new optional field 83). Omission still selects system ConPTY. A shared
`OnceLock` freezes selection before `init_session` can spawn panes; reloads,
new panes, and attaching clients cannot replace the session's provider.
Diagnostic `ZELLIJ_CONPTY_PROVIDER` overrides KDL; normal selection needs no
environment variables. This Windows-only startup option is not a live UI setting.

Verification after this change:

- `zellij-utils --lib`: **1061 passed**, 2 ignored, including invalid KDL values,
  both-value serialization, merge precedence and the IPC round trip. Reviewed
  snapshots add only the new option/default comments (plus snapshot metadata).
- Windows backend: **53 passed, 8 ignored per provider**. Graphics diagnostics:
  **2 passed per provider**; bundled retained exact SIXEL and all three terminal
  queries at flags 0. Fallback rejection test: **1 passed**.
- Pane/grid suite: **729 passed**, 1 ignored, including the SIXEL padding tests.
- Default-feature debug builds produced both `zellij.exe` and `zellij-window.exe`.
- Two real Windows Terminal sessions used KDL **without either provider
  environment variable**. The bundled session loaded the adjacent verified
  `1.22.2507.30001` pair with flags 0; current-main `ya env` reported Zellij,
  `sixel: true`, `csi_16t: (10,20)` and Sixel. After changing its config file to
  system, a new pane in that existing session still used bundled. A separately
  started system-configured session logged kernel32 / flags `0xe` and reported
  `sixel: false`. This is selection/query evidence, not a repeat of image UI tests.
- Both diagnostic sessions were stopped and deleted by exact session name;
  their servers and clients exited. Evidence is under
  `%TEMP%/zellij-kdl-verify-20261025/{bundled,system}` and
  `%TEMP%/zellij-conpty-kdl-*.log`.

The diagnostic pair was checked against the original NuGet cache:
`Microsoft.Windows.Console.ConPTY` **1.22.250730001**, from Microsoft's
TerminalDependencies feed. Both files have valid Microsoft signatures,
file version **1.22.2507.30001**, and identical SHA-256 hashes to the cached pair.
The package metadata declares MIT; the installer retains the original license
and notices. User installation and config activation are a separate approved
follow-up to these checks.

## Reproduce (PowerShell, parent directory containing both repositories)

```powershell
cargo build --manifest-path yazi/Cargo.toml -p yazi-cli -p yazi-fm
$env:ZELLIJ_DIAG_YA = (Resolve-Path yazi/target/debug/ya.exe).Path
cargo test --manifest-path zellij/Cargo.toml -p zellij-server --lib graphics_diagnostics -- --ignored --nocapture --test-threads=1

$old = $env:ZELLIJ_CONPTY_NO_PASSTHROUGH
try {
    $env:ZELLIJ_CONPTY_NO_PASSTHROUGH = '1'
    cargo test --manifest-path zellij/Cargo.toml -p zellij-server --lib graphics_transport_observation -- --ignored --nocapture
} finally {
    $env:ZELLIJ_CONPTY_NO_PASSTHROUGH = $old
}
```

To obtain actual attached-terminal detection, run the freshly built `ya.exe env` directly in Windows Terminal, and again inside a Zellij pane; save the reports to separate files. These reports complement, but do not replace, an actual image-display test.
