# Windows stability integration

Status: core integration implemented; not a fully validated release.

Scope decision: after the core integration, the user approved follow-up watcher, PSReadLine, Windows UI, and Linux default-feature validation. The latest follow-up below supersedes earlier outstanding-item lists. User profiles/modules remain unchanged. The user explicitly accepted the reproduced PowerShell 5.1 non-BMP limitation, chose assisted manual Windows UI verification, and authorized the WSL desktop-test packages.

## Pinned sources

- Upstream: c7c4ce42b8663dad464c03b92a7949ad58c68c55
- Fork: b14c8a3ff4ca06e88a55e992dd5775b2b2cb38e8
- rmux reference: 1f4571e74f36be0c033c6294d1616c7d3a6fbda1
- Worktree: D:/Git/zw; branch: integration/windows-stability
- Original main and untracked yazi.txt remain untouched.

## Fork commit disposition

The behavior-level dispositions below have now been implemented and checked with focused and platform library tests. Test cases are consolidated around behavior rather than retaining every old test function name. This does not imply completion of all release/interactive validation; the input-stall worker and its results are detailed below.

| Commits | Disposition |
| --- | --- |
| eb99a1f60, b56a18ff3 | Retain ConPTY flags/fallback and tests |
| fbaaf34ed, 6067e8c16 | Rework close-after-exit for descendant lifetime; retain regression coverage |
| 05ca3fc20, 6b495e640 | Retain Job Object ownership and process-tree tests |
| 8c396cd4f, 7f64aad12, e96326886, ac3fff101, 8fa56a7c4 | Adapt lossless initial PowerShell DSR bootstrap and tests |
| 73892a39b, dacff8f3a, dbb09b50e, 52270448f | Retain console Unicode writer and flush tests |
| a84d2f31e, cdf0b21d9, ccff735ca, 5f0d771b3, e09487a06 | Adapt idempotent local/remote cleanup alongside upstream console-mode restore |
| a7037e8d9, c31efb62c, f36193bf9 | Formatting absorbed into corresponding logical changes |
| db3167486, 0ef551873 | Retain SIGINT through pane stdin |
| 88b881cc5, e3ed549f2 | Retain fresh-ConPTY fallback on spawn failure |
| f98247175, 6900e8d10, 58abc2373, a0187ee79, 46b6a5404 | Retain bounded blocking reader and real ConPTY coverage |
| b7cc89060, baa1e1284, 3687944da, 354fec719, 1b187d8eb, aa37e8471 | Adapt generation/gap/subscriber/render protection as one feature |
| b14c8a3ff | Upstream already has nonfatal/idempotent screen detach behavior (closed-plugin-channel test added). Preserve native modifier events and literal text rather than the old CSI-u synthesis heuristic; preserve upstream WT/VT parser. Port opt-in bounded input tracing. |

## rmux candidate disposition

- 15365111: adapted bounded input stall recovery after inspecting the implementation and follow-ups; see the worker and licensing details below.
- 0eb4a30f: descendant-liveness behavior is now implemented independently using Job accounting (not rmux console enumeration). Explicit and natural descendant exit tests pass, including the bounded input worker's close/cancellation coverage described below.
- 5084329b, 4aea4051: regression scenarios first; do not duplicate rmux input protocol.
- 444c3b3d: the fork already handled E_HANDLE/broken-pipe resize races, but not every HRESULT. Corrected earlier duplicate classification: added ERROR_INVALID_PARAMETER HRESULT to the benign exit-race set, with a narrow policy test.
- 28c4ba6c: not needed for the anonymous output reader. Revisited for the upcoming cancellable input writer: rmux uses a separate synchronous-read/overlapped-write named pipe, so same-user ACL, unpredictable names and local-only allocation are relevant to that input path.
- 57b0b432 and endpoint rotation: retain interprocess IPC; regression tests rather than implementation copy.

Record source attribution and license review with any rmux-derived implementation.

## Implemented so far

- Console output is routed through `windows_console_writer.rs` on Windows, retaining redirected byte output and incomplete UTF-8 across flushes. Existing prefix/flush tests were retained and all Korean/emoji byte boundaries are covered by an additional test.
- `terminal_cleanup.rs` arbitrates local/remote panic, error, and normal exit cleanup. Shared guards prevent repeated/reentrant teardown; failed raw-mode restoration no longer prevents remaining best-effort cleanup. Latest upstream grapheme, reminder, nested-session, and VT behavior remains in place.
- Four cleanup arbitration tests cover cloned guards, concurrent exits, reentrancy, and independent clients.
- Windows backend now uses RAII-owned anonymous pipes, a bounded blocking output reader (64 x 64KiB), interruptible teardown backpressure, and lossless initial PowerShell DSR filtering across chunk/EOF/timeout boundaries. The worker drains abandoned output to avoid ClosePseudoConsole deadlock.
- Windows build-sensitive ConPTY flags and the existing environment overrides are preserved. A failed passthrough spawn retries with a fresh console/pipes. Upstream quoting, PaneEnv overlays and window startup-handle configuration remain intact.
- Children start suspended and are assigned to a kill-on-close Job before resuming. Job accounting keeps the pane alive after wrapper exit; natural exit waits for the output consumer to finish before delivering the exit callback. Explicit close terminates the tree and releases backpressure. This independently implements the behavior motivating rmux 0eb4a30f; no rmux source was copied.
- SIGINT is written as ETX to the matching pane stdin, without terminating unrelated processes as fallback. Resize exit-race handling includes invalid-handle, broken-pipe and invalid-parameter HRESULTs.
- The global terminals mutex is no longer held during I/O or console destruction. The bounded input worker now isolates per-pane writes and flush barriers, with explicit stall and cancellation handling detailed below.
- Real ConPTY tests cover cmd/PowerShell output, pane environments, >4MiB output, other-pane isolation, wrapper/descendant lifetime, final-output-before-exit ordering, explicit close and failed spawn. A separate 5MiB anonymous-pipe test verifies exact byte preservation because ConPTY can legitimately repeat its final screen frame.
- Common PTY output now carries an invalidatable generation token and sequence through screen delivery and pending-pane queues. All four reader launch paths share one start/stop implementation; close/rerun/drop invalidate before aborting. Tests reject queued stale bytes even before the replacement reader emits anything.
- Bounded per-pane history retains 1024 sequence records and at most 1MiB recent bytes, not duplicate event payloads. Subscriber cursors detect eviction, internal sequence holes and generation resets; recovery resends a complete current viewport without replaying a possibly partial VT suffix. Parser delivery remains lossless for current-generation output.
- Subscriber initial-send/update/close failures remove the subscriber; invalid resubscription removes previous state; pane close removes cursors/viewports/history. Server render forwarding filters disconnected clients/watchers for both ANSI and structured Frame payloads. Detach uses NormalDetached.
- Native input keeps upstream INPUT_RECORD modifier handling. The fork's heuristic reconstructing CSI-u from literal characters was intentionally not copied: text like `[113;5u` could become Ctrl-Q. Regression tests preserve that literal text, actual Ctrl modifiers, and key-release filtering. WT uses upstream's raw VT parser, which does not need this heuristic.
- `ZELLIJ_INPUT_TRACE=1` enables bounded hexadecimal diagnostics at VT/native/server-key boundaries. It is disabled by default and may expose passwords/pasted secrets; do not enable in normal operation.
- Job-restricted launch now retries permitted breakaway for access-denied/quota errors. The isolated subprocess test allows the parent plus ConPTY host but no ordinary child: normal spawn fails, breakaway succeeds. Reallocating the host during this retry caused a real quota race; only passthrough fallback recreates the console.

## Input-stall integration

- The Windows backend now exposes nonblocking retry/acknowledgement to upstream's per-terminal scheduler. Queue admission is NOT acknowledged as an OS write; only completed bytes advance the scheduler. Each pane has a bounded 64-message worker queue and 4KiB transfers.
- rmux-derived overlapped input uses its 2s no-progress + 500ms recovery policy. Recovery keeps the SAME outstanding operation alive instead of cancelling/resubmitting possibly exposed bytes. Permanent timeout stops that pane's input with an explicit error. CancelIoEx is always followed by completion drain before releasing buffers or OVERLAPPED memory; a completion that wins cancellation is acknowledged.
- Input uses a private UUID named pipe with protected current-user-only DACL, non-inheritable handles, first-instance enforcement, remote-client rejection and connected-process identity validation. Output remains an anonymous blocking pipe.
- DSR, SIGINT and focus events use the same worker FIFO without stealing the scheduler's user-write acknowledgement. A bounded ordered barrier replaces blocking FlushFileBuffers. Queue overflow reports an error; the Windows scheduler retains its outstanding head instead of replacing it. A scheduler cancellation acknowledgement removes old pending input before terminal-ID reuse.
- Explicit pane close cancels and joins input before closing ConPTY; root-process exit alone still does not terminate descendants. Natural full-Job exit preserves output EOF consumption.
- Adapted sources include RMUX Authors attribution and the complete MIT notice in `zellij-server/src/windows_input.LICENSE` (rmux 1f4571e7; changes 28c4ba6c/15365111). No new crate dependency or user-facing CLI/config/IPC option was introduced.

## Unicode and IPC verification

- Added exact-byte large bracketed paste/Unicode delivery through the real Windows input pipe, a cross-platform IPC endpoint restart/reconnect test with large Unicode messages, and real ConPTY shell input tests splitting every UTF-8 byte boundary.
- Headless shell checks: cmd and PowerShell 7 preserve Korean + emoji; Windows PowerShell 5.1 preserves Korean, and its cooked input (PSReadLine removed only in the test child) also preserves emoji. The test emulates later cursor-position queries, normally handled by Zellij's grid.
- **Known external compatibility failure:** Windows PowerShell 5.1 with installed PSReadLine 2.0.0 drops emoji keyboard input while preserving Korean. Reproduced with passthrough both enabled and disabled. Removing PSReadLine in the isolated child makes the same bytes pass; PowerShell 7 / PSReadLine 2.4.5 passes. No user module/profile was modified. An explicitly ignored compatibility probe retains this known failing case; this is NOT a passed/default-shell emoji check. Logs: D:/Git/zw-winps-legacy-readline-test.log, zw-winps-unicode-fallback-test.log, zw-winps-cooked-unicode-test.log.
- Console decoder now retains an incomplete Unicode suffix even after earlier invalid UTF-8 bytes; added regression coverage.

## Remaining validation

- The release build passed. Default-feature and no-default-feature workspace runs reached the GUI tests, where the same two watcher cases hung; neither full workspace run is a pass. Actual Windows Terminal/native-console interactive matrix remains unrun. Headless ConPTY shell checks are not a substitute for these UI checks.
- Resolve or explicitly accept the external PSReadLine 2.0.0 non-BMP input limitation before declaring the complete shell matrix verified.

## Verification ledger

Windows MSVC, repository-pinned Rust 1.95.0 (baseline and integration results distinguished below):

- `cargo test -p zellij-server --lib --no-default-features --no-run`: PASS (initial dependency compilation timed out at 200s; retry completed). Six upstream warnings.
- `cargo test -p zellij-server --lib --no-default-features os_input_output_windows -- --test-threads=1`: PASS, 6 tests.
- Original upstream full server library baseline: 2479 passed, 180 ignored.
- Integrated server library with `--no-default-features`: 2519 passed, 181 ignored (including closed-plugin-channel teardown). Log: D:/Git/zw-server-tests.log. The extra ignored test is an isolated subprocess helper executed by the passing Job-breakaway parent test; the other 180 ignores are inherited.
- Output/gap/subscriber tests, including closed-plugin teardown: 17 passed. Log: D:/Git/zw-output-tests.log.
- Job breakaway regression: PASS after correcting the host-reallocation quota race. Log: D:/Git/zw-job-breakaway-tests.log. Earlier failed attempts remain reflected in the old focused log; the subsequent full library run is the current result.
- Real cmd output/EOF with `ZELLIJ_CONPTY_NO_PASSTHROUGH=1`: PASS. Log: D:/Git/zw-conpty-fallback-tests.log.
- Integrated client library tests with `--no-default-features`: PASS, 186 passed, 0 failed/ignored. Log: D:/Git/zw-client-tests.log.
- Input trace utility tests: 3 passed. Log: D:/Git/zw-input-trace-tests.log.
- Changed Rust files: targeted rustfmt check PASS; git diff --check PASS.
- Bounded-input integration: Windows backend/pipe/worker suite 42 passed; one ignored subprocess helper is executed by its parent test. Scheduler suite 10 passed. Logs: D:/Git/zw-input-all-windows-tests.log, D:/Git/zw-input-scheduler-tests.log.
- Eight repeated Windows backend suites with 4 test threads: all passed (336 test executions), including real ConPTY stall/other-pane responsiveness, cancellation/drain, wrapper descendants, output-before-exit, repeated handle cleanup and restrictive-parent-Job breakaway. Logs: D:/Git/zw-conpty-race-{1..8}.log.
- First full workspace no-default-feature run built successfully but failed the new legacy PSReadLine emoji probe (2536 server tests passed, 1 failed). This failure was isolated as described above; the passing cooked-input check is not relabelled as default PSReadLine compatibility. A release build was deliberately interrupted to investigate it.
- Latest Windows workspace runs (both default and no-default features): server 2539 passed / 183 ignored; client 297 passed (workspace feature unification); utils 1059 passed / 2 ignored. Both full runs were stopped at two hanging, unchanged GUI watcher tests, so these are component passes, NOT workspace passes. Logs: D:/Git/zw-workspace-tests-final.log, zw-workspace-default-tests.log.
- Standalone client no-default features: 187 passed. Optional installed PowerShell 7 Unicode probe: passed. Debug WASM build of all bundled plugin packages: passed. Logs: D:/Git/zw-client-final.log, zw-pwsh-unicode-final.log, zw-plugin-debug-build.log.
- Default-feature GUI tests with exactly the two known watcher hangs excluded: 1304 passed, 1 ignored, 2 filtered out. Log: D:/Git/zw-window-tests-excluding-watchers.log. The earlier golden replay failures came from CRLF checkout conversion in five raw terminal-stream fixtures; restoring their exact HEAD LF bytes made the golden test pass. No fixture content change is staged or present in the Git diff.
- Default-feature release build `cargo build --release -p zellij --bin zellij`: PASS (22m48s). `--version` reports `zellij 0.46.0`; `--help` exits successfully. Artifact: `D:/Git/zw/target/release/zellij.exe`; SHA-256 `7C24D0654392FAC59A51C9E9E1930746AB6A6C8C331CD71CC32BA41458290620`. Logs: D:/Git/zw-release-build-final.log, zw-release-help.log.
- Final changed-file rustfmt check and git diff --check: PASS. No staged changes. Original `D:/Git/zellij/zellij` remains on main at b14c8a3ff with only its pre-existing untracked yazi.txt. User profiles/modules and the original branch were not modified. No test/build process was left running.
- Post-input-integration Linux rerun: client 180 / server 2519 passed, using vendored_curl and persistent `/home/lhslhg/.cache/zellij-integration-target`; log D:/Git/zw-linux-tests.log. Separate Linux IPC socket suite, including endpoint restart, passed; log D:/Git/zw-linux-ipc-tests.log.
- Real shell Unicode checks (cmd, Windows PowerShell Korean, Windows PowerShell cooked Korean+emoji, PowerShell 7 Korean+emoji): 4 passed. Windows IPC restart: 1 passed. Logs: D:/Git/zw-unicode-shell-matrix.log, zw-ipc-restart-test.log.
- Ubuntu-24.04 WSL no-default-feature client build initially failed because system OpenSSL development files were absent. No OS packages were installed.
- Retried Linux with the repository's existing `zellij-utils/vendored_curl` feature and isolated `/tmp/zellij-windows-integration-target`: client 180 passed; server 2519 passed, 179 ignored. Logs: `/tmp/zellij-integration-client-vendored-tests-retry.log`, `/tmp/zellij-integration-server-tests.log` (in WSL). The combined command timed out during client compilation; the separate client retry passed. These are not default-feature/workspace/release verification.
- Windows Terminal shell matrix/native-console interactive checks: NOT RUN.

Logs outside the worktree: D:/Git/zw-baseline-server.log and D:/Git/zw-baseline-server-retry.log.

## Follow-up validation (2026-10-24)

### Windows watcher lifetime and full workspace

The two watcher tests closed their reply pipe before the fake server had accepted it. On Windows an early disconnect can restart the server's accept loop. A temporary 100ms delay before reply accept reproduced both hangs; keeping the connection alive until `FakeServer::finish()` fixed them under that delay. The permanent tests use a deferred-accept gate rather than timing, and the fake server now has a 10-second completion deadline. No production watcher behavior changed.

Both `cargo test --workspace --no-default-features` and `cargo test --workspace` completed with exit 0, including the watcher tests. The GUI suites passed 1289 and 1306 tests respectively, each with one existing ignored test. Logs: `D:/Git/zw-workspace-watchers-fixed-no-default.log` and `D:/Git/zw-workspace-watchers-fixed-default.log`. These full runs preceded the additional ignored PSReadLine compatibility probe below.

### PowerShell 5.1 with current PSReadLine: known limitation, not fixed

PSReadLine **2.4.5**, the current stable version listed by [PowerShell Gallery](https://www.powershellgallery.com/packages/PSReadLine/2.4.5), was explicitly imported from the existing PowerShell 7 installation into a separate Windows PowerShell 5.1 `-NoProfile -NoExit` child. No modules were installed or user profiles edited. The probe disables history saving and prediction in that child.

- UTF-8 byte-by-byte `한글끝`: **PASS**.
- UTF-8 byte-by-byte `한글😀끝`: **FAIL**; file contents were `한글�끝` (U+FFFD in place of the emoji), even with PSReadLine 2.4.5.
- Control: PowerShell 5.1 with PSReadLine removed (cooked input): **PASS** for `한글😀끝`.
- Control: PowerShell 7: **PASS** for `한글😀끝`.

The user accepted recording this host/input-stack compatibility limitation instead of implementing an external workaround. Prefer PowerShell 7 for interactive Unicode input; 5.1 cooked input is the verified alternative, without PSReadLine editing features. Upgrading PSReadLine alone is **not** a verified fix. These results establish file-content fidelity, not IME composition or glyph rendering in an actual terminal UI.

Reproduce the opt-in probe (expected to fail on the observed 5.1 host):

```powershell
$env:ZELLIJ_TEST_PSREADLINE_MANIFEST = 'C:/Program Files/PowerShell/7/Modules/PSReadLine/PSReadLine.psd1'
cargo test -p zellij-server windows_powershell_selected_readline_unicode_survives_split_utf8 -- --ignored --nocapture
```

Logs: `D:/Git/zw-winps51-psreadline245.log` (exit 101), `D:/Git/zw-winps51-cooked-confirm.log` (exit 0), and `D:/Git/zw-pwsh-unicode-confirm.log` (exit 0). The ignored legacy probe remains a diagnostic, not a passing compatibility claim.

### Actual WT observations and console-mode regression

The user's WT runs with pwsh, cmd, and Windows PowerShell 5.1 cooked input reported `restored=False`. Their detach/quit JSON records consistently show input mode **487 (0x1E7) -> 480 (0x1E0)**, with output mode 7 and input/output code pages 65001 unchanged; exit codes were 0. The user observed visible screen restoration in pwsh and 5.1 cooked, but that does not establish full state restoration. Native-console visual checks are still outstanding.

A private-console kernel regression reproduced the loss before the fix (`D:/Git/zw-console-mode-before.log`, exit 101). `enable_vt_input()` had captured the mode after raw setup cleared processed/line/echo bits. The client now captures the entire mode **before** raw setup, for native as well as VT input; both panic hooks now disable raw mode before restoring the original mode. The regression covers VT/native × initially cooked/raw, repeated raw entry, two cycles, and idempotent restoration. It passed after the fix (`D:/Git/zw-console-mode-after.log`). A separate opt-in PowerShell-only no-op diagnostic preserved 487 -> 487; the failure was not dismissed as a host baseline artifact.

Both full Windows workspace variants passed again after the fix: `zw-windows-console-fixed-no-default-retry.log` and `zw-windows-console-fixed-default.log` (exit 0). The first attempt hit the tool's 10-minute deadline after 9m20s of compilation and is not counted as complete. The corrected no-default-feature release build and CLI help/version all passed (`D:/Git/zw-windows-console-status.txt`). Binary: `D:/Git/zw/target/release/zellij.exe`; SHA256 `9F55CA9198D6A15365D054B201EC2606016814F92A4BA510D8945B7FF4C2ADD8`. The three generated shell configs also passed `setup --check` with this binary. `cargo install --path . --locked --force --no-default-features` built successfully but could not replace the in-use Cargo executable (Windows error 5; `zw-console-mode-install.log`). No existing client/server was stopped. The installed binary remains the earlier `7C24D0...` version; manual commands explicitly select the corrected build. Installation replacement and actual UI sign-off remain outstanding. Rechecking task #7 confirmed that the existing installation uses **default features and both `zellij.exe` and `zellij-window.exe`**. The no-default candidate must not replace that feature set. A matching default-feature `cargo build --release --locked -p zellij --bins` is now running (`D:/Git/zw-default-modefix-status.txt`); the tested no-default binary is preserved at `target/ui-artifacts/zellij-no-default-9f55ca.exe`.

The original cmd paste procedure also had a harness defect: `set /p` read only 1023 of the 4101 UTF-16 code units. The corrected helper copies a safe long `echo` command and compares the UTF-8 output file exactly (including CRLF). Generated scripts parsed successfully and the command/file comparison returned True/exit 0 without changing the clipboard. This validates the procedure, not actual terminal pasting. Native cases now clear inherited `TERM`/`WT_SESSION` only in the test child to exercise INPUT_RECORD rather than accidentally reusing WT's VT input path.

### Corrected-binary user observations

With release SHA256 `9F55CA...`, WT pwsh (`zw-ui-0153c6f6e7ff`), cmd (`zw-ui-2d8604968e17`), and WinPS cooked (`zw-ui-ba2c1bebe264`) all recorded detach **and** quit exit 0 / `restored=true`: input mode 487 -> 487, output mode 7 unchanged, both code pages 65001 unchanged. This confirms the originally reported mode regression is fixed in those runs, not all visual requirements.

The cmd paste comparison failed because `pasted.txt` was absent (expected UTF-16 length including CRLF: 4103). The user explicitly accepted proceeding on the basis that cmd works normally and treating the fixture issue as nonblocking. Its cause is not proven and the automated paste check is **not** marked passed. Native-console and remaining visual sign-off are still outstanding.

The clean Linux run completed: unit suite, dev-opt executable, doctests, default-feature release and CLI help/version all exit 0. Integration nextest: **768 passed, 1 failed, 1 timed out, 1 skipped** (770 tests run, exit 100). Both failures recur in an isolated serial rerun: the 4MB prompt search reaches `39` but exceeds its no-progress deadline before `39999`; the window-settings test times out awaiting another theme. A separate pristine upstream `c7c4ce42` build, using identical Wasm guests, root+integration feature unification, dev-opt, serial execution and unchanged deadlines, reproduced **both** cases: prompt timeout 60.037s and the same theme wait failure at 21.242s (exit 100; `D:/Git/zw-linux-upstream-retry.log`). The initial comparison command omitted the root package and hit missing non-vendored OpenSSL headers; it was corrected to match the original feature set before this comparison. Only generated dev-opt workspace cache artifacts were invalidated to prevent stale path reuse; the verified integration release/debug outputs and original repositories were preserved. This comparison provides no evidence that either failure was introduced by the integration. They remain failed checks, not a green suite, pending the user's decision to accept pre-existing/environmental limitations or investigate further.

### Remaining follow-up

- Windows Terminal/native console: assisted manual verification approved; WT mode restoration verified, broader UI matrix incomplete. The helper [`zellij-integration-tests/manual/windows-ui.ps1`](zellij-integration-tests/manual/windows-ui.ps1) and [six-case instructions](zellij-integration-tests/manual/windows-ui.md) are ready. PowerShell syntax and three generated configs passed, but initial user runs exposed a helper bug: multiple `Get-Command` matches were concatenated into one executable path. The helper now uses `$PSHOME/pwsh.exe`; the regenerated layout contains a single existing path. It keeps the default socket namespace paired with the normal session index and creates only uniquely named test sessions. Orphaned test servers were identified by their exact test paths and stopped. The user rerun found the input-mode failure described above. Corrected-binary WT runs have since confirmed mode restoration, and the user accepted the cmd fixture issue as nonblocking. Native and remaining visual checks are still needed; helper preparation does not count as UI validation. The helper records binary SHA256 and detach/quit console-mode/code-page comparisons; IME, rendering, resize, and visible restoration still require user observations.
- Linux default features: build headers/libraries downloaded into a user cache without system changes. With separate user approval, Ubuntu-24.04 WSL desktop runtime packages (`xvfb`, `libxkbcommon-x11-0`, `libegl1`, `libgl1-mesa-dri`) were installed. The initial default-feature debug/release builds, workspace doctests, and release CLI help/version all passed. A whole-workspace nextest run finished with 6630 passed / 29 failed / 188 skipped; all 18 required Xvfb desktop smoke tests passed. Of the failures, 22 web unit tests conflicted on their shared SQLite DB when process isolation bypassed their in-process `#[serial]` lock; one desktop asset assertion compared CRLF with LF; six were integration tests (including another CRLF assertion). This is not a passing full suite. Revalidation used a native Linux LF snapshot (HEAD `8e2f482a` plus the current tracked diff and identical Wasm assets), using `cargo test` for unit tests and nextest only for integration tests. The first native-source rerun reused stale artifacts from the old path (including its `CARGO_MANIFEST_DIR`), so it was stopped and is not a clean-snapshot result. All workspace-package artifacts were then cleaned; LF assets were explicitly checked. The clean whole-workspace unit run passed (exit 0), as did the `dev-opt` binary build. The clean integration run used CI's `dev-opt` profile and had the two failures detailed above; the same two failures were reproduced in an isolated upstream comparison (see above). Status: `D:/Git/zw-linux-clean-status.txt`; logs: `D:/Git/zw-linux-clean-*.log`. The structured-web-client detach expectation was updated to the intentionally ported `NormalDetached` contract, retaining parity with local IPC. Ordinary shared-process full-workspace `cargo test` attempts also had integration-test failures/timeouts and are not counted as passing.
