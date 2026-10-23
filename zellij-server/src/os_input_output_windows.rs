use crate::os_input_output::{resolve_command, AsyncReader, PaneEnv};
use crate::panes::PaneId;

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io,
    os::windows::ffi::OsStrExt,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
};

#[path = "windows_conpty.rs"]
mod windows_conpty;
#[path = "windows_conpty_reader.rs"]
mod windows_conpty_reader;
#[path = "windows_input.rs"]
mod windows_input;
#[path = "windows_input_pipe.rs"]
mod windows_input_pipe;
#[path = "windows_job.rs"]
mod windows_job;
use windows_conpty::{Console, PASSTHROUGH};
use windows_input::InputWriter;
use windows_job::Job;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows_sys::Win32::System::Console::{ResizePseudoConsole, COORD, HPCON};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
    PROCESS_TERMINATE, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use zellij_utils::{errors::prelude::*, input::command::RunCommand};

// Not exported by windows-sys; value from the Windows SDK.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;
/// Drop the Job before the console: descendants must stop before pipe teardown.
struct ConPtyTerminal {
    job: Job,
    console: Mutex<Console>,
    input: Arc<InputWriter>,
    child_pid: u32,
    closing: Arc<AtomicBool>,
}

impl ConPtyTerminal {
    fn close(&self) {
        self.closing.store(true, Ordering::Release);
        if let Err(error) = self.job.terminate() {
            log::debug!("Pane job teardown: {error}");
        }
        self.console.lock().unwrap().close_after_exit();
    }

    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        self.input.try_write(buf)
    }
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

/// Encode a Rust string as null-terminated UTF-16.
fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Quote a single argument per the `CommandLineToArgvW` convention.
///
/// Rules:
///   * n backslashes not followed by a quote are n literal backslashes.
///   * 2n backslashes followed by a quote produce n backslashes and a
///     delimiting quote.
///   * 2n+1 backslashes followed by a quote produce n backslashes and
///     a literal quote.
fn append_quoted_arg(cmdline: &mut String, arg: &str) {
    if !(arg.is_empty() || arg.contains(' ') || arg.contains('\t') || arg.contains('"')) {
        cmdline.push_str(arg);
        return;
    }
    cmdline.push('"');
    let mut backslashes: usize = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                cmdline.push('\\');
            }
            backslashes = 0;
            cmdline.push('"');
        } else {
            for _ in 0..backslashes {
                cmdline.push('\\');
            }
            backslashes = 0;
            cmdline.push(ch);
        }
    }
    // Double any trailing backslashes so the closing quote still terminates
    // the argument rather than being escaped.
    for _ in 0..(backslashes * 2) {
        cmdline.push('\\');
    }
    cmdline.push('"');
}

/// Build a Windows command-line string from a `RunCommand`, following the
/// `CommandLineToArgvW` quoting convention.
fn build_command_line_string(cmd: &RunCommand) -> String {
    let mut cmdline = String::new();

    // Executable — always quote to handle spaces in paths.
    let exe = cmd.command.to_string_lossy();
    cmdline.push('"');
    cmdline.push_str(&exe);
    cmdline.push('"');

    for arg in &cmd.args {
        cmdline.push(' ');
        append_quoted_arg(&mut cmdline, arg);
    }

    cmdline
}

fn build_command_line(cmd: &RunCommand) -> Vec<u16> {
    to_wide(&build_command_line_string(cmd))
}

/// Build a UTF-16 environment block (each entry `KEY=VALUE\0`, terminated by
/// an extra `\0`) from the current process environment, adding
/// `ZELLIJ_PANE_ID`.
fn build_environment_block(terminal_id: u32, pane_env: &PaneEnv) -> Vec<u16> {
    let mut variables: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("ZELLIJ_PANE_ID"))
        .collect();
    for (key, value) in pane_env {
        variables.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
        if let Some(value) = value {
            variables.insert(key.clone(), value.clone());
        }
    }
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in variables {
        let entry = format!("{}={}", key, value);
        block.extend(OsStr::new(&entry).encode_wide());
        block.push(0);
    }
    let pane_entry = format!("ZELLIJ_PANE_ID={}", terminal_id);
    block.extend(OsStr::new(&pane_entry).encode_wide());
    block.push(0);
    block.push(0); // double-null terminator
    block
}

/// Spawn a child process attached to the given ConPTY.
///
/// Returns `(process_handle, thread_handle, child_pid)`.
fn spawn_child_process(
    hpcon: HPCON,
    cmd: &RunCommand,
    terminal_id: u32,
    pane_env: &PaneEnv,
    breakaway: bool,
) -> io::Result<(HANDLE, HANDLE, u32)> {
    // --- proc thread attribute list ---
    let mut attr_size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
    }
    let mut attr_buf = vec![0u8; attr_size];
    let attr_list = attr_buf.as_mut_ptr().cast();

    if unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) } == 0 {
        return Err(io::Error::last_os_error());
    }

    // N.B. For PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, lpValue is the HPCON
    // value itself (not a pointer to it). In C, HPCON is `void*` so passing
    // it directly as PVOID is natural. In Rust, HPCON is `isize`, so we cast
    // the value to a pointer. This matches the Microsoft ConPTY sample.
    // See: https://github.com/microsoft/terminal/issues/6705
    if unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            hpcon as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        return Err(io::Error::last_os_error());
    }

    // --- startup info ---
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Null standard handles make the child use the pseudo console. Without this flag a child
    // takes the server's own standard handles whenever those are redirected (as they are when
    // the server is started without a console), and a shell reading a NUL stdin exits at once.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.lpAttributeList = attr_list;

    // --- command line & environment ---
    let mut cmd_line = build_command_line(cmd);
    let env_block = build_environment_block(terminal_id, pane_env);

    let cwd: Option<Vec<u16>> = cmd.cwd.as_ref().and_then(|p| {
        if p.exists() && p.is_dir() {
            Some(to_wide(&p.to_string_lossy()))
        } else {
            log::error!(
                "CWD for new pane '{}' does not exist or is not a directory",
                p.display()
            );
            None
        }
    });
    let cwd_ptr = cwd.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),      // lpApplicationName
            cmd_line.as_mut_ptr(), // lpCommandLine (mutable)
            std::ptr::null(),      // lpProcessAttributes
            std::ptr::null(),      // lpThreadAttributes
            0,                     // bInheritHandles = FALSE
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_SUSPENDED
                | if breakaway {
                    CREATE_BREAKAWAY_FROM_JOB
                } else {
                    0
                },
            env_block.as_ptr().cast(),              // lpEnvironment
            cwd_ptr,                                // lpCurrentDirectory
            &si.StartupInfo as *const STARTUPINFOW, // lpStartupInfo
            &mut pi,                                // lpProcessInformation
        )
    };

    unsafe { DeleteProcThreadAttributeList(attr_list) };

    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok((pi.hProcess, pi.hThread, pi.dwProcessId))
}

fn spawn_managed_child(
    hpcon: HPCON,
    cmd: &RunCommand,
    id: u32,
    env: &PaneEnv,
    breakaway: bool,
) -> io::Result<(OwnedHandle, u32, Job)> {
    let (process, thread, pid) = spawn_child_process(hpcon, cmd, id, env, breakaway)?;
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
    let handle = process.as_raw_handle() as HANDLE;
    let setup = (|| -> io::Result<Job> {
        let job = Job::new()?;
        job.assign(handle)?;
        if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    })();
    match setup {
        Ok(job) => Ok((process, pid, job)),
        Err(error) => {
            unsafe {
                TerminateProcess(handle, 1);
            }
            Err(error)
        },
    }
}

fn is_benign_resize_after_exit_hresult(hr: i32) -> bool {
    // E_HANDLE, broken pipe, and invalid parameter can race process exit.
    matches!(hr as u32, 0x80070006 | 0x8007006d | 0x80070057)
}

fn terminate_process(pid: u32) -> std::result::Result<(), std::io::Error> {
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let ok = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// WindowsPtyBackend
// ---------------------------------------------------------------------------

/// Windows PTY backend with bounded blocking ConPTY output.
#[derive(Clone)]
pub(crate) struct WindowsPtyBackend {
    terminals: Arc<Mutex<BTreeMap<u32, Option<Arc<ConPtyTerminal>>>>>,
    next_terminal_id_counter: Arc<AtomicU32>,
}

impl WindowsPtyBackend {
    pub fn new() -> Result<Self, io::Error> {
        Ok(Self {
            terminals: Arc::new(Mutex::new(BTreeMap::new())),
            next_terminal_id_counter: Arc::new(AtomicU32::new(0)),
        })
    }

    /// Core spawn logic — creates ConPTY, spawns child, sets up exit monitor.
    fn do_spawn(
        &self,
        cmd: RunCommand,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        terminal_id: u32,
        pane_env: &PaneEnv,
    ) -> Result<(Box<dyn AsyncReader>, u32)> {
        let err_context = |c: &RunCommand| {
            format!(
                "failed to spawn terminal for '{}'",
                c.command.to_string_lossy()
            )
        };

        let bootstrap = cmd
            .command
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|name| {
                name.eq_ignore_ascii_case("pwsh") || name.eq_ignore_ascii_case("powershell")
            })
            .unwrap_or(false);
        let mut flags = windows_conpty::selected_flags();
        let (console, reader, process, child_pid, job, input) = loop {
            let (console, reader) =
                Console::create(80, 24, flags, bootstrap).with_context(|| {
                    format!(
                        "allocate ConPTY for {} (flags {flags:#x})",
                        cmd.command.display()
                    )
                })?;
            let input = console.input.clone();
            // Reuse the allocated host for a permitted Job breakaway retry.
            // Creating a second host can itself exceed the enclosing Job quota
            // while the first host is still asynchronously exiting.
            let spawned = spawn_managed_child(console.hpcon, &cmd, terminal_id, pane_env, false)
                .or_else(|error| {
                    if matches!(error.raw_os_error(), Some(5 | 1816)) {
                        log::debug!("Pane Job setup denied: {error}; retry permitted breakaway");
                        spawn_managed_child(console.hpcon, &cmd, terminal_id, pane_env, true)
                    } else {
                        Err(error)
                    }
                });
            match spawned {
                Ok((process, pid, job)) => break (console, reader, process, pid, job, input),
                Err(error) if console.flags & PASSTHROUGH != 0 => {
                    log::debug!(
                        "Passthrough spawn failed: {error}; recreate ConPTY without passthrough"
                    );
                    flags = windows_conpty::fallback_flags(console.flags).unwrap();
                },
                Err(error) => return Err(error).with_context(|| err_context(&cmd)),
            }
            // Passthrough fallback always gets fresh console resources.
            drop(reader);
            drop(console);
        };
        let terminal = Arc::new(ConPtyTerminal {
            job,
            closing: console.discard_signal(),
            console: Mutex::new(console),
            input,
            child_pid,
        });
        let watched = Arc::downgrade(&terminal);
        let output_consumed = reader.consumed_signal();
        let replaced = self
            .terminals
            .lock()
            .unwrap()
            .insert(terminal_id, Some(terminal));
        if let Some(Some(previous)) = replaced {
            previous.close();
        } // outside global lock

        let monitor = std::thread::Builder::new()
            .name("zellij-conpty-exit".into())
            .spawn(move || {
                let handle = process.as_raw_handle() as HANDLE;
                let exit_code = unsafe {
                    WaitForSingleObject(handle, INFINITE);
                    let mut code = 0;
                    if GetExitCodeProcess(handle, &mut code) != 0 {
                        Some(code as i32)
                    } else {
                        None
                    }
                };
                // A wrapper may exit while its console descendants still run.
                // Keep the Job and ConPTY alive until the entire tree finishes.
                loop {
                    let Some(terminal) = watched.upgrade() else {
                        return;
                    };
                    if terminal.closing.load(Ordering::Acquire) {
                        return;
                    }
                    match terminal.job.active_processes() {
                        Ok(0) => {
                            terminal.console.lock().unwrap().close_after_exit();
                            // Publish pane exit only after the terminal parser has
                            // consumed the tail (or abandoned the reader).
                            while !output_consumed.load(Ordering::Acquire)
                                && !terminal.closing.load(Ordering::Acquire)
                            {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            if !terminal.closing.load(Ordering::Acquire) {
                                quit_cb(PaneId::Terminal(terminal_id), exit_code, cmd);
                            }
                            return;
                        },
                        Ok(_) => {},
                        Err(error) => {
                            // Do not silently close a potentially live descendant.
                            log::error!("Cannot query pane Job liveness: {error}");
                            return;
                        },
                    }
                    drop(terminal);
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            });
        if let Err(error) = monitor {
            self.clear_terminal_id(terminal_id);
            return Err(error).context("failed to start ConPTY exit monitor");
        }
        Ok((Box::new(reader), child_pid))
    }

    pub fn spawn_terminal(
        &self,
        mut cmd: RunCommand,
        failover_cmd: Option<RunCommand>,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        terminal_id: u32,
        pane_env: &PaneEnv,
    ) -> Result<(Box<dyn AsyncReader>, u32)> {
        if let Some(resolved) = resolve_command(&cmd, pane_env) {
            cmd.command = resolved;
            return self.do_spawn(cmd, quit_cb, terminal_id, pane_env);
        }
        if let Some(mut failover) = failover_cmd {
            if let Some(resolved) = resolve_command(&failover, pane_env) {
                failover.command = resolved;
                return self.do_spawn(failover, quit_cb, terminal_id, pane_env);
            }
        }
        Err(ZellijError::CommandNotFound {
            terminal_id,
            command: cmd.command.to_string_lossy().to_string(),
        })
        .context("failed to spawn terminal")
    }

    pub fn set_terminal_size(
        &self,
        terminal_id: u32,
        cols: u16,
        rows: u16,
        _width_in_pixels: Option<u16>,
        _height_in_pixels: Option<u16>,
    ) -> Result<()> {
        let err_context = || {
            format!(
                "failed to set terminal {} to size ({}, {})",
                terminal_id, cols, rows
            )
        };

        match self.terminal(terminal_id) {
            Some(term) => {
                if cols > 0 && rows > 0 {
                    let console = term.console.lock().unwrap();
                    if console.hpcon == 0 {
                        return Ok(());
                    }
                    let size = COORD {
                        X: cols.min(i16::MAX as u16) as i16,
                        Y: rows.min(i16::MAX as u16) as i16,
                    };
                    let hr = unsafe { ResizePseudoConsole(console.hpcon, size) };
                    if hr != S_OK && !is_benign_resize_after_exit_hresult(hr) {
                        Err::<(), _>(anyhow!("ResizePseudoConsole failed: HRESULT 0x{:08x}", hr))
                            .with_context(err_context)
                            .non_fatal();
                    }
                }
            },
            _ => {
                Err::<(), _>(anyhow!("no ConPTY terminal found for id {}", terminal_id))
                    .with_context(err_context)
                    .non_fatal();
            },
        }
        Ok(())
    }

    pub fn write_to_tty_stdin(&self, terminal_id: u32, buf: &[u8]) -> Result<usize> {
        let err_context = || format!("failed to write to stdin of terminal {}", terminal_id);

        match self.terminal(terminal_id) {
            Some(term) => term.write(buf).with_context(err_context),
            _ => Err(anyhow!("no ConPTY terminal found for id {}", terminal_id))
                .with_context(err_context),
        }
    }

    pub fn tcdrain(&self, terminal_id: u32) -> Result<()> {
        self.terminal(terminal_id)
            .ok_or_else(|| anyhow!("no ConPTY terminal found for id {}", terminal_id))?
            .input
            .drain()
            .with_context(|| format!("failed to drain terminal {terminal_id}"))
    }

    pub fn write_terminal_control(&self, terminal_id: u32, bytes: &[u8]) -> Result<()> {
        self.terminal(terminal_id)
            .ok_or_else(|| anyhow!("no ConPTY terminal found for id {}", terminal_id))?
            .input
            .enqueue_control(bytes)
            .context("failed to queue terminal control")
    }

    fn terminal(&self, id: u32) -> Option<Arc<ConPtyTerminal>> {
        self.terminals
            .lock()
            .unwrap()
            .get(&id)
            .and_then(Clone::clone)
    }

    fn terminal_for_pid(&self, pid: u32) -> Option<Arc<ConPtyTerminal>> {
        self.terminals
            .lock()
            .unwrap()
            .values()
            .flatten()
            .find(|term| term.child_pid == pid)
            .cloned()
    }

    pub fn kill(&self, pid: u32) -> Result<()> {
        if let Some(term) = self.terminal_for_pid(pid) {
            term.job
                .terminate()
                .context("failed to terminate pane job")?;
        } else {
            terminate_process(pid).with_context(|| format!("failed to kill pid {}", pid))?;
        }
        Ok(())
    }

    pub fn force_kill(&self, pid: u32) -> Result<()> {
        self.kill(pid)
    }

    pub fn send_sigint(&self, pid: u32) -> Result<()> {
        let term = self
            .terminal_for_pid(pid)
            .ok_or_else(|| anyhow!("no pane for pid {}", pid))?;
        term.input
            .enqueue_control(b"\x03")
            .context("failed to queue SIGINT")
    }

    pub fn reserve_terminal_id(&self, terminal_id: u32) {
        let previous = self.terminals.lock().unwrap().insert(terminal_id, None);
        if let Some(Some(previous)) = previous {
            previous.close();
        }
    }

    pub fn clear_terminal_id(&self, terminal_id: u32) {
        let previous = self.terminals.lock().unwrap().remove(&terminal_id);
        if let Some(Some(previous)) = previous {
            previous.close();
        }
    }

    pub fn next_terminal_id(&self) -> Option<u32> {
        Some(
            self.next_terminal_id_counter
                .fetch_add(1, Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
#[path = "windows_conpty_tests.rs"]
mod conpty_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cmd(exe: &str, args: &[&str]) -> RunCommand {
        RunCommand {
            command: PathBuf::from(exe),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_arg_is_left_unquoted() {
        let s = build_command_line_string(&cmd("nvim", &["hello"]));
        assert_eq!(s, "\"nvim\" hello");
    }

    #[test]
    fn arg_with_space_is_quoted() {
        let s = build_command_line_string(&cmd("nvim", &["hello world"]));
        assert_eq!(s, "\"nvim\" \"hello world\"");
    }

    // Regression: zellij edit with a Windows backslash path plus a space in
    // the path used to eat every backslash inside the quoted arg. See
    // https://github.com/zellij-org/zellij/issues/5293.
    #[test]
    fn backslash_path_with_space_preserves_backslashes() {
        let path = r"C:\Users\me\Folder With Space\README.md";
        let s = build_command_line_string(&cmd("nvim", &[path]));
        assert_eq!(
            s,
            "\"nvim\" \"C:\\Users\\me\\Folder With Space\\README.md\""
        );
    }

    #[test]
    fn embedded_quote_is_escaped_with_odd_backslashes() {
        // Input arg: a"b (contains a literal quote). It should be emitted
        // as a\"b inside a quoted arg.
        let s = build_command_line_string(&cmd("prog", &["a\"b"]));
        assert_eq!(s, "\"prog\" \"a\\\"b\"");
    }

    #[test]
    fn backslash_before_embedded_quote_gets_doubled() {
        // Input arg: a\"b — one literal backslash, then a literal quote.
        // Must emit 3 backslashes then a quote so CommandLineToArgvW parses
        // back to a\"b.
        let s = build_command_line_string(&cmd("prog", &["a\\\"b c"]));
        assert_eq!(s, "\"prog\" \"a\\\\\\\"b c\"");
    }

    #[test]
    fn trailing_backslash_before_closing_quote_gets_doubled() {
        // "path with space\" would let the trailing backslash escape the
        // closing quote. The trailing backslash must be doubled.
        let s = build_command_line_string(&cmd("prog", &["C:\\my path\\"]));
        assert_eq!(s, "\"prog\" \"C:\\my path\\\\\"");
    }
}
