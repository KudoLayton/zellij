//! Kernel mode checks run in private, windowless consoles, never the user's console.
use super::*;
use crate::os_input_output::{get_cli_client_os_input, ClientOsApi};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Console::{
    SetStdHandle, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
};

#[test]
fn console_modes_survive_raw_input_and_reentry() {
    let directory = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for case in ["vt-cooked", "vt-raw", "native-cooked", "native-raw"] {
        let log_path = directory.path().join(format!("{case}.log"));
        let log = std::fs::File::create(&log_path).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stdin_handler_windows::console_mode_tests::isolated_console_mode_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("ZELLIJ_CONSOLE_MODE_PROBE", case)
            // CREATE_NO_WINDOW allocates a separate hidden console for this child.
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("private console probe timed out: {case}");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if !status.success() {
            failures.push(format!(
                "{case}: {}",
                std::fs::read_to_string(log_path).unwrap()
            ));
        } else {
            eprintln!("{case}: full mode restored across two cycles and repeated raw entry");
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
#[ignore = "private-console subprocess helper, invoked by console_modes_survive_raw_input_and_reentry"]
fn isolated_console_mode_probe() {
    let case = std::env::var("ZELLIJ_CONSOLE_MODE_PROBE").expect("run via the parent test");
    let input = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
        .expect("the subprocess must have its own hidden console");
    let handle = input.as_raw_handle();
    // Rust's test harness inherits redirected stdio. Use this child's console input.
    assert_ne!(unsafe { SetStdHandle(STD_INPUT_HANDLE, handle) }, 0);
    let cooked_bits = ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT;
    let original = 0x01e0
        | if case.ends_with("-cooked") {
            cooked_bits
        } else {
            0
        };
    assert_ne!(unsafe { SetConsoleMode(handle, original) }, 0);
    let mut os = get_cli_client_os_input().unwrap();
    for _ in 0..2 {
        os.set_raw_mode();
        if case.starts_with("vt-") {
            assert!(enable_vt_input());
        } else {
            // Native mouse capture also changes mode flags after raw mode is set.
            assert_ne!(
                unsafe {
                    SetConsoleMode(
                        handle,
                        ENABLE_EXTENDED_FLAGS | ENABLE_MOUSE_INPUT | ENABLE_WINDOW_INPUT,
                    )
                },
                0
            );
        }
        // A reconnect can enter raw mode again without first returning to the shell.
        os.set_raw_mode();
        // Shared teardown order: first disable raw, then restore the complete original mode.
        os.unset_raw_mode().unwrap();
        os.restore_console_mode();
        let mut restored = 0;
        assert_ne!(unsafe { GetConsoleMode(handle, &mut restored) }, 0);
        assert_eq!(
            restored, original,
            "{case}: full input mode must be restored"
        );
        os.restore_console_mode();
        assert_ne!(unsafe { GetConsoleMode(handle, &mut restored) }, 0);
        assert_eq!(restored, original, "restoration must be idempotent");
    }
}
