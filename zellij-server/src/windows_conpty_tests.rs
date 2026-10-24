//! Real Windows kernel/ConPTY regressions. No external terminal UI required.
use super::*;
use std::time::Duration;

struct Pane {
    backend: WindowsPtyBackend,
    reader: Box<dyn AsyncReader>,
    pid: u32,
    exit: std::sync::mpsc::Receiver<Option<i32>>,
}

impl Pane {
    fn spawn(command: &str, args: &[&str], env: PaneEnv) -> Self {
        let backend = WindowsPtyBackend::new().unwrap();
        let (tx, exit) = std::sync::mpsc::channel();
        let (reader, pid) = backend
            .spawn_terminal(
                RunCommand {
                    command: command.into(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
                None,
                Box::new(move |_, code, _| {
                    let _ = tx.send(code);
                }),
                1,
                &env,
            )
            .unwrap();
        Self {
            backend,
            reader,
            pid,
            exit,
        }
    }

    async fn write(&self, bytes: &[u8]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut offset = 0;
            while offset < bytes.len() {
                match self
                    .backend
                    .write_to_tty_stdin(1, &bytes[offset..])
                    .unwrap()
                {
                    0 => tokio::time::sleep(Duration::from_millis(1)).await,
                    n => offset += n,
                }
            }
        })
        .await
        .expect("ConPTY write deadline");
    }

    async fn until(&mut self, marker: &[u8]) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut bytes = Vec::new();
            loop {
                let chunk = self.reader.read_chunk(8192).await.unwrap();
                assert!(
                    !chunk.is_empty(),
                    "EOF before marker: {}",
                    String::from_utf8_lossy(&bytes)
                );
                bytes.extend(chunk);
                if bytes.windows(marker.len()).any(|w| w == marker) {
                    return bytes;
                }
            }
        })
        .await
        .expect("ConPTY marker deadline")
    }

    async fn eof(&mut self) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut bytes = Vec::new();
            loop {
                let chunk = self.reader.read_chunk(32768).await.unwrap();
                if chunk.is_empty() {
                    return bytes;
                }
                bytes.extend(chunk);
            }
        })
        .await
        .expect("ConPTY EOF deadline")
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.backend.clear_terminal_id(1);
    }
}

#[tokio::test]
async fn real_cmd_output_reaches_eof_after_exit() {
    let mut pane = Pane::spawn(
        "cmd.exe",
        &["/d", "/c", "echo conpty-real-output"],
        PaneEnv::default(),
    );
    let output = pane.eof().await;
    assert!(String::from_utf8_lossy(&output).contains("conpty-real-output"));
    assert_eq!(
        pane.exit.recv_timeout(Duration::from_secs(3)).unwrap(),
        Some(0)
    );
}

#[tokio::test]
async fn real_powershell_bootstrap_and_pane_environment_are_preserved() {
    let mut env = PaneEnv::default();
    env.insert(
        "ZELLIJ_CONPTY_TEST_VALUE".into(),
        Some("pane-env-marker".into()),
    );
    let mut pane = Pane::spawn("powershell.exe", &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
        "[Console]::WriteLine($env:ZELLIJ_CONPTY_TEST_VALUE); [Console]::WriteLine('ps-finished')"], env);
    let output = pane.eof().await;
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("pane-env-marker"), "{text}");
    assert!(text.contains("ps-finished"), "{text}");
}

async fn unicode_input_roundtrip(shell: &str, args: &[&str], cmd: bool, expected: &str) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unicode.txt");
    let command = if cmd {
        format!(
            "chcp 65001 >nul\r\necho {expected}>\"{}\"\r\n",
            path.display()
        )
    } else {
        format!(
            "[IO.File]::WriteAllText('{}','{expected}',[Text.Encoding]::UTF8)\r\n",
            path.display().to_string().replace('\'', "''")
        )
    };
    let mut pane = Pane::spawn(shell, args, PaneEnv::default());
    pane.until(b">").await;
    // Exercise every UTF-8 boundary through real WriteFile/ConPTY decoding.
    for byte in command.as_bytes() {
        pane.write(&[*byte]).await;
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut queries = Vec::new();
        loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if text.trim_start_matches('\u{feff}').trim_end() == expected {
                    break;
                }
            }
            // Act as the attached terminal: PSReadLine can request cursor
            // position again after its initial bootstrap. The production grid
            // answers these queries; this headless test has no grid/host UI.
            tokio::select! {
                chunk = pane.reader.read_chunk(8192) => {
                    let chunk = chunk.unwrap();
                    assert!(!chunk.is_empty(), "shell exited before Unicode roundtrip");
                    queries.extend(chunk);
                    let count = queries.windows(4).filter(|bytes| *bytes == b"\x1b[6n").count();
                    for _ in 0..count { pane.backend.write_terminal_control(1, b"\x1b[1;1R").unwrap(); }
                    if queries.len() > 3 { queries.drain(..queries.len() - 3); }
                },
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{shell} Unicode input mismatch: {:?}", std::fs::read(&path)));
}

#[tokio::test]
#[ignore = "optional PowerShell 7 host-mode diagnostic; no Zellij client is launched"]
async fn pwsh_native_command_console_mode_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("baseline.ps1");
    let result = directory.path().join("baseline.json");
    std::fs::write(
        &script,
        r#"Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class BaselineConsole {
    [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int which);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetConsoleMode(IntPtr h, out uint mode);
    public static uint Mode(int which) {
        uint mode;
        if (!GetConsoleMode(GetStdHandle(which), out mode)) throw new Win32Exception();
        return mode;
    }
}
'@
Write-Host 'BASELINE_READY'
$null = Read-Host 'start'
$before = [BaselineConsole]::Mode(-10)
& $env:ComSpec /d /c exit 0
$after = [BaselineConsole]::Mode(-10)
[IO.File]::WriteAllText($env:ZELLIJ_CONSOLE_BASELINE_PATH, (@{before=$before; after=$after} | ConvertTo-Json))
"#,
    )
    .unwrap();
    let mut env = PaneEnv::default();
    env.insert(
        "ZELLIJ_CONSOLE_BASELINE_PATH".into(),
        Some(result.display().to_string()),
    );
    let mut pane = Pane::spawn(
        "pwsh.exe",
        &["-NoLogo", "-NoProfile", "-File", script.to_str().unwrap()],
        env,
    );
    pane.until(b"BASELINE_READY").await;
    pane.write(b"\r").await;
    pane.eof().await;
    assert_eq!(
        pane.exit.recv_timeout(Duration::from_secs(3)).unwrap(),
        Some(0)
    );
    let observation: serde_json::Value =
        serde_json::from_slice(&std::fs::read(result).unwrap()).unwrap();
    assert!(observation["before"].as_u64().is_some());
    assert!(observation["after"].as_u64().is_some());
    eprintln!("PowerShell native no-op input modes (no Zellij): {observation}");
}

#[tokio::test]
async fn cmd_unicode_input_survives_split_utf8() {
    unicode_input_roundtrip("cmd.exe", &["/d", "/q"], true, "한글😀끝").await;
}

#[tokio::test]
async fn windows_powershell_hangul_input_survives_split_utf8() {
    unicode_input_roundtrip(
        "powershell.exe",
        &["-NoLogo", "-NoProfile"],
        false,
        "한글끝",
    )
    .await;
}

#[tokio::test]
#[ignore = "known Windows PowerShell 5.1 non-BMP limitation; explicit compatibility probe"]
async fn windows_powershell_legacy_readline_non_bmp_compatibility() {
    unicode_input_roundtrip(
        "powershell.exe",
        &["-NoLogo", "-NoProfile"],
        false,
        "한글😀끝",
    )
    .await;
}

#[tokio::test]
#[ignore = "set ZELLIJ_TEST_PSREADLINE_MANIFEST to a compatible PSReadLine 2.4.5+ manifest"]
async fn windows_powershell_selected_readline_unicode_survives_split_utf8() {
    let manifest = std::env::var("ZELLIJ_TEST_PSREADLINE_MANIFEST")
        .expect("provide an explicit PSReadLine manifest; user modules are not changed");
    assert!(std::path::Path::new(&manifest).is_file());
    let directory = tempfile::tempdir().unwrap();
    let version_file = directory.path().join("version.txt");
    let setup = format!(
        "$ErrorActionPreference='Stop'; Import-Module '{}' -Force; \
         $v=(Get-Module PSReadLine).Version; \
         if ($PSVersionTable.PSVersion.Major -ne 5 -or $v -lt [version]'2.4.5') {{ throw 'unexpected host/module version' }}; \
         Set-PSReadLineOption -HistorySaveStyle SaveNothing -PredictionSource None; \
         [IO.File]::WriteAllText('{}',$v.ToString())",
        manifest.replace('\'', "''"),
        version_file.display().to_string().replace('\'', "''"),
    );
    for text in ["한글끝", "한글😀끝"] {
        unicode_input_roundtrip(
            "powershell.exe",
            &["-NoLogo", "-NoProfile", "-NoExit", "-Command", &setup],
            false,
            text,
        )
        .await;
        let version = std::fs::read_to_string(&version_file).unwrap();
        eprintln!("Windows PowerShell 5.1 / PSReadLine {version}: {text} roundtrip passed");
    }
}

#[tokio::test]
async fn windows_powershell_cooked_input_survives_split_utf8() {
    unicode_input_roundtrip(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NoExit",
            "-Command",
            "Remove-Module PSReadLine -ErrorAction SilentlyContinue",
        ],
        false,
        "한글😀끝",
    )
    .await;
}

#[tokio::test]
#[ignore = "optional installed PowerShell 7; run explicitly where pwsh.exe is available"]
async fn pwsh_unicode_input_survives_split_utf8() {
    unicode_input_roundtrip("pwsh.exe", &["-NoLogo", "-NoProfile"], false, "한글😀끝").await;
}

#[tokio::test]
async fn real_output_exceeding_queue_capacity_is_delivered() {
    let count = 5 * 1024 * 1024;
    // ConPTY can redraw its last frame on close, duplicating visible characters.
    // Exact byte preservation is tested separately against an anonymous pipe.
    let script = format!("$s=[Console]::OpenStandardOutput(); $b=[Text.Encoding]::ASCII.GetBytes(('Q' * {count})); $s.Write($b,0,$b.Length)");
    let mut pane = Pane::spawn(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ],
        PaneEnv::default(),
    );
    let output = pane.eof().await;
    assert!(output.iter().filter(|byte| **byte == b'Q').count() >= count);
}

#[tokio::test]
async fn explicit_close_ends_output_with_a_live_shell() {
    let mut pane = Pane::spawn("cmd.exe", &["/d", "/q"], PaneEnv::default());
    let command = b"echo ready-to-close\r\n";
    pane.write(command).await;
    pane.until(b"ready-to-close").await;
    pane.backend.clear_terminal_id(1);
    let _ = pane.eof().await;
    assert!(pane.backend.terminal(1).is_none());
}

#[tokio::test]
async fn exit_notification_waits_for_reader_tail() {
    let mut pane = Pane::spawn(
        "cmd.exe",
        &["/d", "/c", "echo final-output-before-exit"],
        PaneEnv::default(),
    );
    // Waiting for the parent handle is an actual completion event, not a sleep.
    let parent = unsafe {
        OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE,
            0,
            pane.pid,
        )
    };
    if !parent.is_null() {
        let parent = unsafe { OwnedHandle::from_raw_handle(parent) };
        assert_eq!(
            unsafe { WaitForSingleObject(parent.as_raw_handle() as HANDLE, 5000) },
            0
        );
    }
    assert!(matches!(
        pane.exit.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    let output = pane.eof().await;
    assert!(String::from_utf8_lossy(&output).contains("final-output-before-exit"));
    assert_eq!(
        pane.exit.recv_timeout(Duration::from_secs(3)).unwrap(),
        Some(0)
    );
}

#[tokio::test]
async fn closing_one_pane_does_not_remove_another() {
    let mut first = Pane::spawn("cmd.exe", &["/d", "/q"], PaneEnv::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let (mut second, _) = first
        .backend
        .spawn_terminal(
            RunCommand {
                command: "cmd.exe".into(),
                args: vec!["/d".into(), "/c".into(), "echo other-pane-alive".into()],
                ..Default::default()
            },
            None,
            Box::new(move |_, code, _| {
                let _ = tx.send(code);
            }),
            2,
            &PaneEnv::default(),
        )
        .unwrap();
    first.backend.clear_terminal_id(1);
    let _ = first.eof().await;
    let output = tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        loop {
            let chunk = second.read_chunk(8192).await.unwrap();
            if chunk.is_empty() {
                break bytes;
            }
            bytes.extend(chunk);
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&output).contains("other-pane-alive"));
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), Some(0));
    first.backend.clear_terminal_id(2);
}

#[tokio::test]
async fn stalled_conpty_input_keeps_other_panes_responsive_and_close_cancels_it() {
    let mut pane = Pane::spawn("powershell.exe", &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
        "[Console]::WriteLine('stall-ready'); $b=[Text.Encoding]::ASCII.GetBytes(('Q' * (32*1024*1024))); [Console]::OpenStandardOutput().Write($b,0,$b.Length); Start-Sleep -Seconds 60"], PaneEnv::default());
    pane.until(b"stall-ready").await;
    let terminal = pane.backend.terminal(1).unwrap();
    // Stop consuming output to create real ConPTY backpressure.
    tokio::time::timeout(Duration::from_secs(12), async {
        while !terminal.input.is_stalled() {
            pane.backend.write_to_tty_stdin(1, &[b'x'; 4096]).unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("real ConPTY input must enter stall recovery");
    let (mut second, _) = pane
        .backend
        .spawn_terminal(
            RunCommand {
                command: "cmd.exe".into(),
                args: vec![
                    "/d".into(),
                    "/c".into(),
                    "echo responsive-second-pane".into(),
                ],
                ..Default::default()
            },
            None,
            Box::new(|_, _, _| {}),
            2,
            &PaneEnv::default(),
        )
        .unwrap();
    let bytes = tokio::time::timeout(Duration::from_secs(3), async {
        let mut bytes = Vec::new();
        loop {
            let chunk = second.read_chunk(8192).await.unwrap();
            if chunk.is_empty() {
                break bytes;
            }
            bytes.extend(chunk);
        }
    })
    .await
    .expect("other pane is not blocked by input stall");
    assert!(String::from_utf8_lossy(&bytes).contains("responsive-second-pane"));
    let at = std::time::Instant::now();
    pane.backend.clear_terminal_id(1);
    assert!(at.elapsed() < Duration::from_secs(3));
    let _ = pane.eof().await;
    pane.backend.clear_terminal_id(2);
}

#[test]
fn sigint_unknown_pid_does_not_force_terminate() {
    let backend = WindowsPtyBackend::new().unwrap();
    assert!(backend.send_sigint(u32::MAX).is_err());
}

#[test]
fn resize_after_exit_error_policy_is_narrow() {
    for hr in [0x80070006_u32, 0x8007006d, 0x80070057] {
        assert!(is_benign_resize_after_exit_hresult(hr as i32));
    }
    assert!(!is_benign_resize_after_exit_hresult(0x80070005_u32 as i32));
}

#[tokio::test]
async fn sigint_does_not_force_terminate_the_shell() {
    let mut pane = Pane::spawn("cmd.exe", &["/d", "/q"], PaneEnv::default());
    pane.write(b"echo before-interrupt\r\n").await;
    pane.until(b"before-interrupt").await;
    pane.backend.send_sigint(pane.pid).unwrap();
    pane.backend.tcdrain(1).unwrap();
    pane.write(b"echo after-interrupt\r\n").await;
    pane.until(b"after-interrupt").await;
    assert!(
        pane.backend
            .terminal(1)
            .unwrap()
            .job
            .active_processes()
            .unwrap()
            > 0
    );
}

#[test]
#[ignore = "isolated subprocess helper, executed by restrictive_parent_job_allows_breakaway"]
fn breakaway_probe() {
    let gate = std::path::PathBuf::from(
        std::env::var_os("ZELLIJ_TEST_JOB_GATE").expect("parent test gate"),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !gate.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut pane = Pane::spawn(
            "cmd.exe",
            &["/d", "/c", "echo breakaway-success"],
            PaneEnv::default(),
        );
        assert!(String::from_utf8_lossy(&pane.eof().await).contains("breakaway-success"));
    });
}

#[test]
fn restrictive_parent_job_allows_breakaway() {
    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("gate");
    let log_path = dir.path().join("probe.log");
    let output = std::fs::File::create(&log_path).unwrap();
    let job = Job::new().unwrap();
    job.restrict_to_console_host_with_breakaway().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "os_input_output_windows::conpty_tests::breakaway_probe",
            "--ignored",
            "--test-threads=1",
        ])
        .env("ZELLIJ_TEST_JOB_GATE", &gate)
        .env("RUST_BACKTRACE", "0")
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap();
    if let Err(error) = job.assign(child.as_raw_handle() as HANDLE) {
        let _ = child.kill();
        let _ = child.wait();
        panic!("assign probe: {error}");
    }
    std::fs::write(gate, "go").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "breakaway probe deadline: {}",
                std::fs::read_to_string(&log_path).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "{}",
        std::fs::read_to_string(log_path).unwrap()
    );
}

#[test]
fn failed_spawn_leaves_no_registered_terminal() {
    let backend = WindowsPtyBackend::new().unwrap();
    let result = backend.do_spawn(
        RunCommand {
            command: "Z:\\zellij-test-missing-command.exe".into(),
            ..Default::default()
        },
        Box::new(|_, _, _| panic!("failed spawn must not publish exit")),
        5,
        &PaneEnv::default(),
    );
    assert!(result.is_err());
    assert!(backend.terminal(5).is_none());
}

#[tokio::test]
async fn descendants_survive_wrapper_exit_and_are_killed_on_pane_close() {
    exercise_descendant_lifetime(false).await;
}

#[tokio::test]
async fn descendant_final_output_precedes_natural_pane_exit() {
    exercise_descendant_lifetime(true).await;
}

async fn exercise_descendant_lifetime(natural_exit: bool) {
    let dir = tempfile::tempdir().unwrap();
    let child = dir.path().join("child.ps1");
    let pid_path = dir.path().join("child.pid");
    let ready_path = dir.path().join("child.ready");
    let release_path = dir.path().join("child.release");
    std::fs::write(&child, format!(
        "[IO.File]::WriteAllText('{}', [string]$PID); [IO.File]::WriteAllText('{}', 'ready'); while (-not [IO.File]::Exists('{}')) {{ Start-Sleep -Milliseconds 20 }}; [Console]::WriteLine('descendant-final-output')",
        pid_path.display(), ready_path.display(), release_path.display())).unwrap();
    // cmd.exe has its own /c quoting rules, unlike CommandLineToArgvW.
    // Keep shell syntax in a script rather than passing it as one quoted arg.
    let wrapper = dir.path().join("wrapper.cmd");
    std::fs::write(&wrapper, format!(
        "@echo off\r\nstart \"\" /b powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"\r\n",
        child.display())).unwrap();
    let mut pane = Pane::spawn(
        "cmd.exe",
        &["/d", "/c", wrapper.to_str().unwrap()],
        PaneEnv::default(),
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready_path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("descendant readiness");
    let child_pid: u32 = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
    // Confirm the wrapper exited, not merely that the descendant started.
    let parent = unsafe {
        OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE,
            0,
            pane.pid,
        )
    };
    if !parent.is_null() {
        let parent = unsafe { OwnedHandle::from_raw_handle(parent) };
        assert_eq!(
            unsafe { WaitForSingleObject(parent.as_raw_handle() as HANDLE, 3000) },
            0
        );
    }
    assert!(matches!(
        pane.exit.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    let child_handle = unsafe {
        OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE,
            0,
            child_pid,
        )
    };
    assert!(!child_handle.is_null());
    let child_handle = unsafe { OwnedHandle::from_raw_handle(child_handle) };
    assert_eq!(
        unsafe { WaitForSingleObject(child_handle.as_raw_handle() as HANDLE, 0) },
        258
    );
    if natural_exit {
        std::fs::write(release_path, "exit").unwrap();
        let output = pane.eof().await;
        assert!(String::from_utf8_lossy(&output).contains("descendant-final-output"));
        assert_eq!(
            pane.exit.recv_timeout(Duration::from_secs(3)).unwrap(),
            Some(0)
        );
    } else {
        pane.backend.clear_terminal_id(1);
        // Repeated explicit teardown is harmless.
        pane.backend.clear_terminal_id(1);
        let _ = pane.eof().await;
    }
    assert_eq!(
        unsafe { WaitForSingleObject(child_handle.as_raw_handle() as HANDLE, 3000) },
        0
    );
}
