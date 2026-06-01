use crate::os_input_output::{resolve_command, AsyncReader};
use crate::panes::PaneId;

use std::{
    collections::{BTreeMap, VecDeque},
    env,
    ffi::OsStr,
    io,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle},
    ops::BitOr,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use tokio::io::AsyncReadExt;
use tokio::net::windows::named_pipe::NamedPipeServer;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, S_OK};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, GenerateConsoleCtrlEvent, ResizePseudoConsole, COORD,
    CTRL_C_EVENT, HPCON,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, CreatePipe};
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOEXW;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
    PROCESS_TERMINATE, STARTUPINFOEXW, STARTUPINFOW,
};

use zellij_utils::{errors::prelude::*, input::command::RunCommand};

pub use async_trait::async_trait;

// Not exported by windows-sys; value from the Windows SDK.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x00080000;
const PIPE_ACCESS_INBOUND: u32 = 0x00000001;
const PIPE_TYPE_BYTE: u32 = 0;
const PIPE_WAIT: u32 = 0;
const GENERIC_WRITE: u32 = 0x40000000;
const PASSTHROUGH_MIN_BUILD: u32 = 22_621;
const DISABLE_PASSTHROUGH_ENV: &str = "ZELLIJ_CONPTY_NO_PASSTHROUGH";
const E_HANDLE_HRESULT: i32 = 0x80070006_u32 as i32;
const ERROR_BROKEN_PIPE_HRESULT: i32 = 0x8007006d_u32 as i32;
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_RESPONSE: &[u8] = b"\x1b[1;1R";
const MAX_DSR_BOOTSTRAP_READS: usize = 8;

/// Monotonic counter so each ConPTY output pipe gets a unique name, even when
/// re-spawning on the same `terminal_id` (the old async reader may still hold
/// the previous pipe handle).
static PIPE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-terminal ConPTY state.
struct ConPtyTerminal {
    hpcon: HPCON,
    input_write_handle: HANDLE,
    job_handle: HANDLE,
    child_pid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ConPtyFlags(u32);

impl ConPtyFlags {
    const RESIZE_QUIRK: Self = Self(0x2);
    const WIN32_INPUT_MODE: Self = Self(0x4);
    const PASSTHROUGH_MODE: Self = Self(0x8);

    const fn empty() -> Self {
        Self(0)
    }

    const fn bits(self) -> u32 {
        self.0
    }

    const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for ConPtyFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl ConPtyTerminal {
    fn close_pseudoconsole(&mut self) {
        if self.hpcon != 0 {
            unsafe { ClosePseudoConsole(self.hpcon) };
            self.hpcon = 0;
        }
    }
}

// HANDLE/HPCON are raw pointers in windows-sys >= 0.59; OS handles are
// safe to send across threads.
unsafe impl Send for ConPtyTerminal {}
unsafe impl Sync for ConPtyTerminal {}

impl Drop for ConPtyTerminal {
    fn drop(&mut self) {
        unsafe {
            // Close the pseudo console first — it may write a final VT frame
            // to the output pipe. The async reader task (if still alive) will
            // drain it. Then close the remaining handle.
            self.close_pseudoconsole();
            CloseHandle(self.input_write_handle);
            CloseHandle(self.job_handle);
        }
    }
}

/// An `AsyncReader` backed by a named pipe connected to ConPTY output.
///
/// Construction stores the raw `OwnedHandle`. The first `read()` call promotes
/// it to a `NamedPipeServer` (IOCP registration requires a live Tokio reactor,
/// which is not available at `spawn_terminal` time).
struct ConPtyAsyncReader {
    pending: Option<OwnedHandle>,
    pipe: Option<NamedPipeServer>,
    dsr_bootstrap: Option<DsrBootstrap>,
    buffered_output: VecDeque<u8>,
}

// OwnedHandle is Send+Sync; NamedPipeServer is Send.
// The reader is only ever used from a single async task (via &mut self),
// so Sync is safe.
unsafe impl Sync for ConPtyAsyncReader {}

impl ConPtyAsyncReader {
    fn new(handle: OwnedHandle, dsr_bootstrap: Option<DsrBootstrap>) -> Self {
        Self {
            pending: Some(handle),
            pipe: None,
            dsr_bootstrap,
            buffered_output: VecDeque::new(),
        }
    }

    fn drain_buffered_output(&mut self, buf: &mut [u8]) -> Option<usize> {
        if self.buffered_output.is_empty() {
            return None;
        }
        let bytes_to_copy = buf.len().min(self.buffered_output.len());
        for byte in buf.iter_mut().take(bytes_to_copy) {
            *byte = self.buffered_output.pop_front().unwrap();
        }
        Some(bytes_to_copy)
    }
}

#[async_trait]
impl AsyncReader for ConPtyAsyncReader {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        if let Some(bytes_read) = self.drain_buffered_output(buf) {
            return Ok(bytes_read);
        }
        if let Some(handle) = self.pending.take() {
            let pipe = unsafe { NamedPipeServer::from_raw_handle(handle.into_raw_handle()) }?;
            self.pipe = Some(pipe);
        }
        let pipe = self
            .pipe
            .as_mut()
            .expect("ConPtyAsyncReader used after init");
        loop {
            let bytes_read = pipe.read(buf).await?;
            if bytes_read == 0 {
                return Ok(0);
            }
            if let Some(dsr_bootstrap) = self.dsr_bootstrap.as_mut() {
                let filtered = dsr_bootstrap.filter(&buf[..bytes_read]);
                if dsr_bootstrap.is_done() {
                    self.dsr_bootstrap = None;
                }
                if filtered.is_empty() {
                    continue;
                }
                let bytes_to_copy = filtered.len().min(buf.len());
                buf[..bytes_to_copy].copy_from_slice(&filtered[..bytes_to_copy]);
                self.buffered_output
                    .extend(filtered[bytes_to_copy..].iter().copied());
                return Ok(bytes_to_copy);
            }
            return Ok(bytes_read);
        }
    }
}

struct DsrBootstrap {
    input_write_handle: HANDLE,
    reads_remaining: usize,
    answered: bool,
}

unsafe impl Send for DsrBootstrap {}
unsafe impl Sync for DsrBootstrap {}

impl DsrBootstrap {
    fn new(input_write_handle: HANDLE) -> Self {
        Self {
            input_write_handle,
            reads_remaining: MAX_DSR_BOOTSTRAP_READS,
            answered: false,
        }
    }

    fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.reads_remaining = self.reads_remaining.saturating_sub(1);
        let (filtered, found_dsr) = strip_dsr_query_once(bytes);
        if found_dsr && !self.answered {
            self.answered = true;
            let mut written = 0;
            unsafe {
                WriteFile(
                    self.input_write_handle,
                    DSR_RESPONSE.as_ptr(),
                    DSR_RESPONSE.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                );
            }
        }
        filtered
    }

    fn is_done(&self) -> bool {
        self.answered || self.reads_remaining == 0
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

/// Build a Windows command-line string from a `RunCommand`, following the
/// `CommandLineToArgvW` quoting convention.
fn build_command_line(cmd: &RunCommand) -> Vec<u16> {
    let mut cmdline = String::new();

    // Executable — always quote to handle spaces in paths.
    let exe = cmd.command.to_string_lossy();
    cmdline.push('"');
    cmdline.push_str(&exe);
    cmdline.push('"');

    for arg in &cmd.args {
        cmdline.push(' ');
        if arg.is_empty() || arg.contains(' ') || arg.contains('\t') || arg.contains('"') {
            cmdline.push('"');
            let mut backslashes: usize = 0;
            for ch in arg.chars() {
                if ch == '\\' {
                    backslashes += 1;
                } else if ch == '"' {
                    // Double backslashes preceding a quote, then escape the quote.
                    for _ in 0..backslashes {
                        cmdline.push('\\');
                    }
                    backslashes = 0;
                    cmdline.push('\\');
                    cmdline.push('"');
                } else {
                    backslashes = 0;
                    cmdline.push(ch);
                }
            }
            // Double trailing backslashes before the closing quote.
            for _ in 0..backslashes {
                cmdline.push('\\');
            }
            cmdline.push('"');
        } else {
            cmdline.push_str(arg);
        }
    }

    to_wide(&cmdline)
}

/// Build a UTF-16 environment block (each entry `KEY=VALUE\0`, terminated by
/// an extra `\0`) from the current process environment, adding
/// `ZELLIJ_PANE_ID`.
fn build_environment_block(terminal_id: u32) -> Vec<u16> {
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in std::env::vars() {
        if key == "ZELLIJ_PANE_ID" {
            continue;
        }
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

fn strip_dsr_query_once(bytes: &[u8]) -> (Vec<u8>, bool) {
    if let Some(index) = bytes
        .windows(DSR_QUERY.len())
        .position(|window| window == DSR_QUERY)
    {
        let mut filtered = Vec::with_capacity(bytes.len() - DSR_QUERY.len());
        filtered.extend_from_slice(&bytes[..index]);
        filtered.extend_from_slice(&bytes[index + DSR_QUERY.len()..]);
        (filtered, true)
    } else {
        (bytes.to_vec(), false)
    }
}

fn command_uses_powershell(cmd: &RunCommand) -> bool {
    cmd.command
        .file_stem()
        .and_then(|name| name.to_str())
        .map(|name| {
            let name = name.to_ascii_lowercase();
            name == "pwsh" || name == "powershell"
        })
        .unwrap_or(false)
}

fn child_process_creation_flags(breakaway_from_job: bool) -> u32 {
    let mut flags =
        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
    if breakaway_from_job {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    flags
}

fn select_conpty_flags(windows_build: u32, passthrough_disabled: bool) -> ConPtyFlags {
    let mut flags = ConPtyFlags::RESIZE_QUIRK | ConPtyFlags::WIN32_INPUT_MODE;
    if !passthrough_disabled && windows_build >= PASSTHROUGH_MIN_BUILD {
        flags = flags | ConPtyFlags::PASSTHROUGH_MODE;
    }
    flags
}

fn fallback_conpty_flags(flags: ConPtyFlags) -> Option<ConPtyFlags> {
    if flags.contains(ConPtyFlags::PASSTHROUGH_MODE) {
        Some(ConPtyFlags::RESIZE_QUIRK | ConPtyFlags::WIN32_INPUT_MODE)
    } else if flags.bits() != 0 {
        Some(ConPtyFlags::empty())
    } else {
        None
    }
}

fn is_benign_resize_after_exit_hresult(hr: i32) -> bool {
    matches!(hr, E_HANDLE_HRESULT | ERROR_BROKEN_PIPE_HRESULT)
}

fn selected_conpty_flags() -> ConPtyFlags {
    let passthrough_disabled = env_flag(DISABLE_PASSTHROUGH_ENV);
    let windows_build = current_windows_build().unwrap_or_else(|e| {
        log::debug!("Failed to detect Windows build for ConPTY flags: {}", e);
        0
    });
    select_conpty_flags(windows_build, passthrough_disabled)
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn current_windows_build() -> io::Result<u32> {
    let mut info: OSVERSIONINFOEXW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = size_of::<OSVERSIONINFOEXW>() as u32;
    let status = unsafe { RtlGetVersion(&mut info) };
    if status < 0 {
        Err(io::Error::from_raw_os_error(status))
    } else {
        Ok(info.dwBuildNumber)
    }
}

/// Create an overlapped named-pipe pair for ConPTY output.
///
/// Returns `(server_read_handle, client_write_handle)` where the server
/// (read) end has `FILE_FLAG_OVERLAPPED` for IOCP and the client (write) end
/// is synchronous (required by ConPTY).
fn create_overlapped_output_pipe(terminal_id: u32) -> io::Result<(HANDLE, HANDLE)> {
    let seq = PIPE_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        r"\\.\pipe\zellij-pty-{}-{}-{}",
        std::process::id(),
        terminal_id,
        seq
    );
    let wide_name = to_wide(&name);

    let server = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_WAIT,
            1,     // max instances
            0,     // out buffer (we only read)
            65536, // in buffer
            0,     // default timeout
            std::ptr::null(),
        )
    };
    if server == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    let client = unsafe {
        CreateFileW(
            wide_name.as_ptr(),
            GENERIC_WRITE,
            0,                    // no sharing
            std::ptr::null(),     // default security
            OPEN_EXISTING,        // pipe already exists
            0,                    // synchronous
            std::ptr::null_mut(), // no template
        )
    };
    if client == INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(server) };
        return Err(io::Error::last_os_error());
    }

    Ok((server, client))
}

/// Create a ConPTY pseudo console of the given size attached to the provided
/// pipes.
fn create_conpty(
    cols: u16,
    rows: u16,
    input_read: HANDLE,
    output_write: HANDLE,
) -> io::Result<HPCON> {
    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };
    let mut hpcon: HPCON = 0;
    let mut flags = selected_conpty_flags();
    loop {
        let hr = unsafe {
            CreatePseudoConsole(size, input_read, output_write, flags.bits(), &mut hpcon)
        };
        if hr == S_OK {
            return Ok(hpcon);
        }
        if let Some(fallback_flags) = fallback_conpty_flags(flags) {
            log::debug!(
                "CreatePseudoConsole failed with flags 0x{:x} (HRESULT 0x{:08x}); retrying with 0x{:x}",
                flags.bits(),
                hr,
                fallback_flags.bits()
            );
            flags = fallback_flags;
        } else {
            return Err(io::Error::from_raw_os_error(hr));
        }
    }
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version_information: *mut OSVERSIONINFOEXW) -> i32;
}

fn create_kill_on_close_job() -> io::Result<HANDLE> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut core::ffi::c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        Err(error)
    } else {
        Ok(job)
    }
}

fn assign_process_to_job(job: HANDLE, process: HANDLE) -> io::Result<()> {
    let ok = unsafe { AssignProcessToJobObject(job, process) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn terminate_job(job: HANDLE) -> io::Result<()> {
    let ok = unsafe { TerminateJobObject(job, 1) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Spawn a child process attached to the given ConPTY.
///
/// Returns `(process_handle, thread_handle, child_pid, job_handle)`.
fn spawn_child_process(
    hpcon: HPCON,
    cmd: &RunCommand,
    terminal_id: u32,
) -> io::Result<(HANDLE, HANDLE, u32, HANDLE)> {
    spawn_child_process_attempt(hpcon, cmd, terminal_id, false).or_else(|first_error| {
        log::debug!(
            "Failed to assign child process to job; retrying with CREATE_BREAKAWAY_FROM_JOB: {}",
            first_error
        );
        spawn_child_process_attempt(hpcon, cmd, terminal_id, true)
    })
}

fn spawn_child_process_attempt(
    hpcon: HPCON,
    cmd: &RunCommand,
    terminal_id: u32,
    breakaway_from_job: bool,
) -> io::Result<(HANDLE, HANDLE, u32, HANDLE)> {
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
    si.lpAttributeList = attr_list;

    // --- command line & environment ---
    let mut cmd_line = build_command_line(cmd);
    let env_block = build_environment_block(terminal_id);

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
    let job = create_kill_on_close_job()?;

    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),      // lpApplicationName
            cmd_line.as_mut_ptr(), // lpCommandLine (mutable)
            std::ptr::null(),      // lpProcessAttributes
            std::ptr::null(),      // lpThreadAttributes
            0,                     // bInheritHandles = FALSE
            child_process_creation_flags(breakaway_from_job),
            env_block.as_ptr().cast(),              // lpEnvironment
            cwd_ptr,                                // lpCurrentDirectory
            &si.StartupInfo as *const STARTUPINFOW, // lpStartupInfo
            &mut pi,                                // lpProcessInformation
        )
    };

    unsafe { DeleteProcThreadAttributeList(attr_list) };

    if ok == 0 {
        unsafe { CloseHandle(job) };
        return Err(io::Error::last_os_error());
    }

    if let Err(e) = assign_process_to_job(job, pi.hProcess) {
        unsafe {
            TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            CloseHandle(job);
        }
        return Err(e);
    }

    if unsafe { ResumeThread(pi.hThread) } == u32::MAX {
        let error = io::Error::last_os_error();
        let _ = terminate_job(job);
        unsafe {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            CloseHandle(job);
        }
        return Err(error);
    }

    Ok((pi.hProcess, pi.hThread, pi.dwProcessId, job))
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

/// Windows PTY backend using native ConPTY with IOCP-based async I/O.
#[derive(Clone)]
pub(crate) struct WindowsPtyBackend {
    terminals: Arc<Mutex<BTreeMap<u32, Option<ConPtyTerminal>>>>,
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
    ) -> Result<(Box<dyn AsyncReader>, u32)> {
        let err_context = |c: &RunCommand| {
            format!(
                "failed to spawn terminal for '{}'",
                c.command.to_string_lossy()
            )
        };

        // 1. Output pipe pair (named, overlapped read end for IOCP)
        let (output_read, output_write) =
            create_overlapped_output_pipe(terminal_id).with_context(|| err_context(&cmd))?;

        // 2. Input pipe pair (anonymous, both synchronous)
        let mut input_read: HANDLE = std::ptr::null_mut();
        let mut input_write: HANDLE = std::ptr::null_mut();
        if unsafe { CreatePipe(&mut input_read, &mut input_write, std::ptr::null(), 0) } == 0 {
            unsafe {
                CloseHandle(output_read);
                CloseHandle(output_write);
            }
            return Err(io::Error::last_os_error()).with_context(|| err_context(&cmd));
        }

        // 3. Create pseudo console
        let hpcon = match create_conpty(80, 24, input_read, output_write) {
            Ok(h) => h,
            Err(e) => {
                unsafe {
                    CloseHandle(output_read);
                    CloseHandle(output_write);
                    CloseHandle(input_read);
                    CloseHandle(input_write);
                }
                return Err(e).with_context(|| err_context(&cmd));
            },
        };

        // 4. ConPTY duplicated the pipe ends it needs; close our copies.
        unsafe {
            CloseHandle(input_read);
            CloseHandle(output_write);
        }

        // 5. Spawn child process
        let (process_handle, thread_handle, child_pid, job_handle) =
            match spawn_child_process(hpcon, &cmd, terminal_id) {
                Ok(r) => r,
                Err(e) => {
                    unsafe {
                        ClosePseudoConsole(hpcon);
                        CloseHandle(input_write);
                        CloseHandle(output_read);
                    }
                    return Err(e).with_context(|| err_context(&cmd));
                },
            };

        // Thread handle is not needed after spawn.
        unsafe { CloseHandle(thread_handle) };

        // 6. Store per-terminal state
        self.terminals.lock().unwrap().insert(
            terminal_id,
            Some(ConPtyTerminal {
                hpcon,
                input_write_handle: input_write,
                job_handle,
                child_pid,
            }),
        );

        // 7. Exit-monitoring thread (zero CPU — spends all time in kernel wait)
        // Pass the HANDLE through as `usize` because raw pointers are not
        // Send. Windows OS handles are safe to use cross-thread.
        let cmd_for_monitor = cmd.clone();
        let process_handle_addr = process_handle as usize;
        let terminals_for_monitor = self.terminals.clone();
        std::thread::spawn(move || {
            let process_handle = process_handle_addr as HANDLE;
            let exit_code = unsafe {
                WaitForSingleObject(process_handle, INFINITE);
                let mut code: u32 = 0;
                GetExitCodeProcess(process_handle, &mut code);
                CloseHandle(process_handle);
                code
            };
            if let Some(Some(term)) = terminals_for_monitor.lock().unwrap().get_mut(&terminal_id) {
                term.close_pseudoconsole();
            }
            quit_cb(
                PaneId::Terminal(terminal_id),
                Some(exit_code as i32),
                cmd_for_monitor,
            );
        });

        // 8. Wrap the output read handle in an async reader
        let owned = unsafe { OwnedHandle::from_raw_handle(output_read as *mut core::ffi::c_void) };
        let dsr_bootstrap = command_uses_powershell(&cmd).then_some(DsrBootstrap::new(input_write));
        let reader = Box::new(ConPtyAsyncReader::new(owned, dsr_bootstrap)) as Box<dyn AsyncReader>;

        Ok((reader, child_pid))
    }

    pub fn spawn_terminal(
        &self,
        mut cmd: RunCommand,
        failover_cmd: Option<RunCommand>,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        terminal_id: u32,
    ) -> Result<(Box<dyn AsyncReader>, u32)> {
        if let Some(resolved) = resolve_command(&cmd) {
            cmd.command = resolved;
            return self.do_spawn(cmd, quit_cb, terminal_id);
        }
        if let Some(mut failover) = failover_cmd {
            if let Some(resolved) = resolve_command(&failover) {
                failover.command = resolved;
                return self.do_spawn(failover, quit_cb, terminal_id);
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

        match self
            .terminals
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(term)) => {
                if cols > 0 && rows > 0 {
                    let size = COORD {
                        X: cols as i16,
                        Y: rows as i16,
                    };
                    let hr = unsafe { ResizePseudoConsole(term.hpcon, size) };
                    if hr != S_OK && is_benign_resize_after_exit_hresult(hr) {
                        log::debug!(
                            "ResizePseudoConsole after terminal {} exit: HRESULT 0x{:08x}",
                            terminal_id,
                            hr
                        );
                    } else if hr != S_OK {
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

        match self
            .terminals
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(term)) => {
                let mut written: u32 = 0;
                let ok = unsafe {
                    WriteFile(
                        term.input_write_handle,
                        buf.as_ptr(),
                        buf.len() as u32,
                        &mut written,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    Err(io::Error::last_os_error()).with_context(err_context)
                } else {
                    Ok(written as usize)
                }
            },
            _ => Err(anyhow!("no ConPTY terminal found for id {}", terminal_id))
                .with_context(err_context),
        }
    }

    pub fn tcdrain(&self, terminal_id: u32) -> Result<()> {
        let err_context = || format!("failed to drain terminal {}", terminal_id);

        match self
            .terminals
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(term)) => {
                let ok = unsafe { FlushFileBuffers(term.input_write_handle) };
                if ok == 0 {
                    // FlushFileBuffers can legitimately fail on pipe handles
                    // (ERROR_INVALID_FUNCTION) — treat as non-fatal.
                    let e = io::Error::last_os_error();
                    log::debug!("FlushFileBuffers on terminal {}: {}", terminal_id, e);
                }
                Ok(())
            },
            _ => Err(anyhow!("no ConPTY terminal found for id {}", terminal_id))
                .with_context(err_context),
        }
    }

    pub fn kill(&self, pid: u32) -> Result<()> {
        if self.terminate_job_for_pid(pid)? {
            return Ok(());
        }
        terminate_process(pid).with_context(|| format!("failed to kill pid {}", pid))?;
        Ok(())
    }

    pub fn force_kill(&self, pid: u32) -> Result<()> {
        if self.terminate_job_for_pid(pid)? {
            return Ok(());
        }
        terminate_process(pid).with_context(|| format!("failed to force-kill pid {}", pid))?;
        Ok(())
    }

    pub fn send_sigint(&self, pid: u32) -> Result<()> {
        let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid) };
        if ok != 0 {
            Ok(())
        } else {
            terminate_process(pid)
                .with_context(|| format!("failed to send SIGINT to pid {}", pid))?;
            Ok(())
        }
    }

    pub fn reserve_terminal_id(&self, terminal_id: u32) {
        self.terminals.lock().unwrap().insert(terminal_id, None);
    }

    pub fn clear_terminal_id(&self, terminal_id: u32) {
        self.terminals.lock().unwrap().remove(&terminal_id);
    }

    pub fn next_terminal_id(&self) -> Option<u32> {
        Some(
            self.next_terminal_id_counter
                .fetch_add(1, Ordering::Relaxed),
        )
    }

    fn terminate_job_for_pid(&self, pid: u32) -> Result<bool> {
        let err_context = || format!("failed to terminate job for pid {}", pid);
        let terminals = self.terminals.lock().to_anyhow().with_context(err_context)?;
        if let Some(term) = terminals
            .values()
            .filter_map(|terminal| terminal.as_ref())
            .find(|terminal| terminal.child_pid == pid)
        {
            terminate_job(term.job_handle).with_context(err_context)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conpty_flags_include_resize_and_win32_input_by_default() {
        let flags = select_conpty_flags(22621, false);

        assert!(flags.contains(ConPtyFlags::RESIZE_QUIRK));
        assert!(flags.contains(ConPtyFlags::WIN32_INPUT_MODE));
        assert!(flags.contains(ConPtyFlags::PASSTHROUGH_MODE));
    }

    #[test]
    fn conpty_flags_skip_passthrough_on_older_windows_builds() {
        let flags = select_conpty_flags(19045, false);

        assert!(flags.contains(ConPtyFlags::RESIZE_QUIRK));
        assert!(flags.contains(ConPtyFlags::WIN32_INPUT_MODE));
        assert!(!flags.contains(ConPtyFlags::PASSTHROUGH_MODE));
    }

    #[test]
    fn conpty_flags_allow_passthrough_to_be_disabled() {
        let flags = select_conpty_flags(22621, true);

        assert!(flags.contains(ConPtyFlags::RESIZE_QUIRK));
        assert!(flags.contains(ConPtyFlags::WIN32_INPUT_MODE));
        assert!(!flags.contains(ConPtyFlags::PASSTHROUGH_MODE));
    }

    #[test]
    fn conpty_flags_fallback_removes_passthrough_first() {
        let flags = ConPtyFlags::RESIZE_QUIRK
            | ConPtyFlags::WIN32_INPUT_MODE
            | ConPtyFlags::PASSTHROUGH_MODE;

        assert_eq!(
            fallback_conpty_flags(flags),
            Some(ConPtyFlags::RESIZE_QUIRK | ConPtyFlags::WIN32_INPUT_MODE)
        );
        assert_eq!(
            fallback_conpty_flags(ConPtyFlags::RESIZE_QUIRK | ConPtyFlags::WIN32_INPUT_MODE),
            Some(ConPtyFlags::empty())
        );
        assert_eq!(fallback_conpty_flags(ConPtyFlags::empty()), None);
    }

    #[test]
    fn resize_after_exit_errors_are_benign() {
        assert!(is_benign_resize_after_exit_hresult(
            0x80070006_u32 as i32
        ));
        assert!(is_benign_resize_after_exit_hresult(
            0x8007006d_u32 as i32
        ));
        assert!(!is_benign_resize_after_exit_hresult(
            0x80070057_u32 as i32
        ));
    }

    #[test]
    fn child_process_spawn_flags_start_suspended_and_can_break_away() {
        assert_eq!(
            child_process_creation_flags(false),
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED
        );
        assert_eq!(
            child_process_creation_flags(true),
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_SUSPENDED
                | CREATE_BREAKAWAY_FROM_JOB
        );
    }

    #[test]
    fn dsr_bootstrap_strips_initial_query_without_losing_output() {
        assert_eq!(
            strip_dsr_query_once(b"hello\x1b[6nworld"),
            (b"helloworld".to_vec(), true)
        );
        assert_eq!(
            strip_dsr_query_once(b"hello world"),
            (b"hello world".to_vec(), false)
        );
    }
}
