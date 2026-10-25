//! Opt-in observations against the real ConPTY backend, not a terminal UI test.
use super::*;

#[tokio::test]
#[ignore = "manual current-main ya env diagnostic; set ZELLIJ_DIAG_YA to built ya.exe"]
async fn graphics_yazi_probe_observation() {
    let ya = std::env::var("ZELLIJ_DIAG_YA").expect("set absolute ZELLIJ_DIAG_YA");
    assert!(std::path::Path::new(&ya).is_file());
    for (respond, inherit_brand) in [(false, true), (true, true), (true, false)] {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("env.txt");
        let script = dir.path().join("probe.cmd");
        std::fs::write(
            &script,
            format!(
                "@echo off\r\n\"{ya}\" env > \"{}\"\r\nexit /b %errorlevel%\r\n",
                report.display()
            ),
        )
        .unwrap();
        let mut env = PaneEnv::default();
        if !inherit_brand {
            for name in [
                "WT_SESSION",
                "TERM",
                "TERM_PROGRAM",
                "KITTY_WINDOW_ID",
                "KONSOLE_VERSION",
                "ITERM_SESSION_ID",
                "WEZTERM_EXECUTABLE",
                "GHOSTTY_RESOURCES_DIR",
                "WARP_HONOR_PS1",
                "VSCODE_INJECTION",
                "TABBY_CONFIG_DIRECTORY",
            ] {
                env.insert(name.into(), None);
            }
        }
        let mut pane = Pane::spawn("cmd.exe", &["/d", "/c", script.to_str().unwrap()], env);
        let (actual_flags, bundled) = {
            let terminal = pane.backend.terminal(1).unwrap();
            let console = terminal.console.lock().unwrap();
            (console.flags, console.is_bundled())
        };
        // Controlled host replies: this isolates ConPTY, not a live Zellij grid.
        let queries: &[(&[u8], &[u8])] = &[
            (b"\x1b[>q", b"\x1bP>|Zellij 0.46.0\x1b\\"),
            (b"\x1b[16t", b"\x1b[6;20;10t"),
            (b"\x1b[c", b"\x1b[?62;4;52c"),
            // Yazi emits the explicit default parameter, not just CSI c.
            (b"\x1b[0c", b"\x1b[?62;4;52c"),
        ];
        let mut answered = vec![0; queries.len()];
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let chunk = pane.reader.read_chunk(8192).await.unwrap();
                if chunk.is_empty() {
                    break;
                }
                bytes.extend(chunk);
                if respond {
                    for (i, (query, reply)) in queries.iter().enumerate() {
                        // A bundled ConPTY also asks DA1 during startup. Reply to
                        // each occurrence, rather than consuming the application's reply.
                        let count = bytes.windows(query.len()).filter(|w| *w == *query).count();
                        while answered[i] < count {
                            pane.backend.write_terminal_control(1, reply).unwrap();
                            answered[i] += 1;
                        }
                    }
                }
            }
        })
        .await
        .expect("ya env deadline");
        let text = std::fs::read_to_string(report).expect("ya env report file");
        assert_eq!(
            pane.exit.recv_timeout(Duration::from_secs(3)).unwrap(),
            Some(0)
        );
        assert!(
            text.contains("Drivers.matches"),
            "ya env did not produce adapter information"
        );
        if bundled && respond {
            let probe = text
                .lines()
                .find(|line| line.contains("Emulator.probe"))
                .unwrap();
            for expected in ["brand: Zellij", "sixel: true", "csi_16t: (10, 20)"] {
                assert!(probe.contains(expected), "missing {expected}: {probe}");
            }
        }
        eprintln!("YA_OBSERVATION bundled={bundled} respond={respond} inherit_brand={inherit_brand} actual_flags={actual_flags} answered={answered:?}");
        for line in text.lines().filter(|line| {
            [
                "Version:",
                "Brand.from_env",
                "Emulator.probe",
                "Drivers.matches",
                "TERM.dimension",
            ]
            .iter()
            .any(|key| line.contains(key))
        }) {
            eprintln!("{line}");
        }
    }
}

#[tokio::test]
#[ignore = "manual SIXEL/query transport diagnostic; requires PowerShell"]
async fn graphics_transport_observation() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("emit.ps1");
    std::fs::write(&script, r#"
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class GraphicsConsole {
    [DllImport("kernel32.dll")] public static extern IntPtr GetStdHandle(int n);
    [DllImport("kernel32.dll")] public static extern bool GetConsoleMode(IntPtr h, out uint mode);
    [DllImport("kernel32.dll")] public static extern bool SetConsoleMode(IntPtr h, uint mode);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool WriteFile(IntPtr h, byte[] bytes, uint count, out uint written, IntPtr overlapped);
    public static void Emit(string value) {
        byte[] bytes = System.Text.Encoding.UTF8.GetBytes(value);
        uint written;
        if (!WriteFile(GetStdHandle(-11), bytes, (uint)bytes.Length, out written, IntPtr.Zero) || written != bytes.Length)
            throw new System.ComponentModel.Win32Exception();
    }
}
'@
$h = [GraphicsConsole]::GetStdHandle(-11)
[uint32]$m = 0
if (-not [GraphicsConsole]::GetConsoleMode($h, [ref]$m)) { throw 'GetConsoleMode' }
if (-not [GraphicsConsole]::SetConsoleMode($h, ($m -bor 4))) { throw 'SetConsoleMode' }
$e = [char]27
[GraphicsConsole]::Emit("GRAPHICS_BEGIN${e}P9;1q`"1;1;1;6#0;2;100;0;0#0~${e}\GRAPHICS_END")
[GraphicsConsole]::Emit("QUERY_BEGIN${e}[c${e}[>q${e}[16tQUERY_END")
"#).unwrap();
    let mut pane = Pane::spawn(
        "powershell.exe",
        &[
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-File",
            script.to_str().unwrap(),
        ],
        PaneEnv::default(),
    );
    let (actual_flags, bundled) = {
        let terminal = pane.backend.terminal(1).unwrap();
        let console = terminal.console.lock().unwrap();
        (console.flags, console.is_bundled())
    };
    let bytes = pane.eof().await;
    assert_eq!(
        pane.exit.recv_timeout(Duration::from_secs(3)).unwrap(),
        Some(0)
    );
    let sixel = b"\x1bP9;1q\"1;1;1;6#0;2;100;0;0#0~\x1b\\";
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(has(b"GRAPHICS_BEGIN") && has(b"GRAPHICS_END") && has(b"QUERY_END"));
    if bundled {
        assert_eq!(actual_flags, 0);
        assert!(has(sixel), "bundled provider did not preserve exact SIXEL");
        for query in [b"\x1b[c".as_slice(), b"\x1b[>q", b"\x1b[16t"] {
            assert!(has(query), "bundled provider did not forward {query:?}");
        }
    }
    eprintln!("OBSERVATION bundled={bundled} actual_flags={actual_flags} selected_flags={} sixel_exact={} da1_forwarded={} xtversion_forwarded={} cell_query_forwarded={} output={:?}",
        windows_conpty::selected_flags(), has(sixel), has(b"\x1b[c"), has(b"\x1b[>q"), has(b"\x1b[16t"), String::from_utf8_lossy(&bytes));
}
