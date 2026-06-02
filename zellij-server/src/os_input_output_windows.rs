use crate::os_input_output::{resolve_command, AsyncReader};
use crate::panes::PaneId;

use std::{
    collections::{BTreeMap, VecDeque},
    env,
    ffi::OsStr,
    io,
    mem::size_of,
    ops::BitOr,
    os::windows::ffi::OsStrExt,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_INVALID_PARAMETER, ERROR_NO_DATA, HANDLE,
    INVALID_HANDLE_VALUE, S_OK,
};
use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, GenerateConsoleCtrlEvent, ResizePseudoConsole, COORD,
    CTRL_C_EVENT, HPCON,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOEXW;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
    PROCESS_TERMINATE, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use zellij_utils::{errors::prelude::*, input::command::RunCommand};

pub use async_trait::async_trait;

// Not exported by windows-sys; value from the Windows SDK.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;
const PASSTHROUGH_MIN_BUILD: u32 = 22_621;
const DISABLE_PASSTHROUGH_ENV: &str = "ZELLIJ_CONPTY_NO_PASSTHROUGH";
const E_HANDLE_HRESULT: i32 = 0x80070006_u32 as i32;
const ERROR_BROKEN_PIPE_HRESULT: i32 = 0x8007006d_u32 as i32;
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_RESPONSE: &[u8] = b"\x1b[1;1R";
const CTRL_C_STDIN: &[u8] = b"\x03";
const DEFAULT_DSR_BOOTSTRAP_TIMEOUT_MS: u64 = 200;
const MIN_DSR_BOOTSTRAP_TIMEOUT_MS: u64 = 50;
const MAX_DSR_BOOTSTRAP_TIMEOUT_MS: u64 = 2_000;
const DSR_BOOTSTRAP_TIMEOUT_ENV: &str = "ZELLIJ_DSR_BOOTSTRAP_TIMEOUT_MS";

const CONPTY_READ_BUFFER_SIZE: usize = 65536;
const CONPTY_OUTPUT_CHANNEL_CAPACITY: usize = 64;

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

    const fn uses_passthrough(self) -> bool {
        self.contains(Self::PASSTHROUGH_MODE)
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

struct ConPtyBlockingReader {
    output_rx: Receiver<io::Result<Vec<u8>>>,
    buffered_output: VecDeque<u8>,
}

unsafe impl Sync for ConPtyBlockingReader {}

impl ConPtyBlockingReader {
    fn new(handle: OwnedHandle, dsr_bootstrap: Option<DsrBootstrap>) -> Self {
        Self::from_receiver(spawn_blocking_conpty_reader(handle, dsr_bootstrap))
    }

    fn from_receiver(output_rx: Receiver<io::Result<Vec<u8>>>) -> Self {
        Self {
            output_rx,
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
impl AsyncReader for ConPtyBlockingReader {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        if let Some(bytes_read) = self.drain_buffered_output(buf) {
            return Ok(bytes_read);
        }

        match self.output_rx.recv().await {
            Some(Ok(bytes)) => {
                let bytes_to_copy = bytes.len().min(buf.len());
                buf[..bytes_to_copy].copy_from_slice(&bytes[..bytes_to_copy]);
                self.buffered_output
                    .extend(bytes[bytes_to_copy..].iter().copied());
                Ok(bytes_to_copy)
            },
            Some(Err(error)) => Err(error),
            None => Ok(0),
        }
    }
}

fn spawn_blocking_conpty_reader(
    handle: OwnedHandle,
    mut dsr_bootstrap: Option<DsrBootstrap>,
) -> Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel(CONPTY_OUTPUT_CHANNEL_CAPACITY);
    std::thread::Builder::new()
        .name("zellij-conpty-reader".to_owned())
        .spawn(move || {
            let mut buffer = vec![0_u8; CONPTY_READ_BUFFER_SIZE];
            loop {
                if let Some(dsr) = dsr_bootstrap.as_mut() {
                    if let Some(bytes) = dsr.drain_deferred(CONPTY_READ_BUFFER_SIZE) {
                        if dsr.is_done() {
                            dsr_bootstrap = None;
                        }
                        if !bytes.is_empty() && tx.blocking_send(Ok(bytes)).is_err() {
                            break;
                        }
                        continue;
                    }
                }

                match read_from_conpty_output(&handle, &mut buffer) {
                    Ok(0) => break,
                    Ok(bytes_read) => {
                        let mut bytes = buffer[..bytes_read].to_vec();
                        if let Some(dsr) = dsr_bootstrap.as_mut() {
                            bytes = dsr.filter_to_len(&bytes, CONPTY_READ_BUFFER_SIZE);
                            if dsr.is_done() {
                                dsr_bootstrap = None;
                            }
                            if bytes.is_empty() {
                                continue;
                            }
                        }
                        if tx.blocking_send(Ok(bytes)).is_err() {
                            break;
                        }
                    },
                    Err(error) => {
                        let _ = tx.blocking_send(Err(error));
                        break;
                    },
                }
            }
        })
        .expect("failed to spawn ConPTY reader thread");
    rx
}

fn read_from_conpty_output(handle: &OwnedHandle, buffer: &mut [u8]) -> io::Result<usize> {
    let mut bytes_read = 0;
    let ok = unsafe {
        ReadFile(
            handle.as_raw_handle() as HANDLE,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut bytes_read,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(bytes_read as usize);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|code| code as u32) {
        Some(code) if code == ERROR_BROKEN_PIPE || code == ERROR_NO_DATA => Ok(0),
        _ => Err(error),
    }
}

struct DsrBootstrap {
    input_write_handle: HANDLE,
    deadline: Instant,
    completed: bool,
    pending: Vec<u8>,
    deferred: VecDeque<u8>,
}

unsafe impl Send for DsrBootstrap {}
unsafe impl Sync for DsrBootstrap {}

impl DsrBootstrap {
    fn new(input_write_handle: HANDLE) -> Self {
        Self {
            input_write_handle,
            deadline: Instant::now() + configured_dsr_bootstrap_timeout(),
            completed: false,
            pending: Vec::new(),
            deferred: VecDeque::new(),
        }
    }

    fn filter_to_len(&mut self, bytes: &[u8], max_len: usize) -> Vec<u8> {
        if self.completed || Instant::now() > self.deadline {
            self.completed = true;
            let output = self.emit_with_pending(bytes);
            return self.defer_output_tail(output, max_len);
        }

        let mut combined = Vec::with_capacity(self.pending.len() + bytes.len());
        combined.extend_from_slice(&self.pending);
        combined.extend_from_slice(bytes);

        if let Some(index) = find_subslice(&combined, DSR_QUERY) {
            self.completed = true;
            self.pending.clear();
            let mut output = Vec::with_capacity(combined.len() - DSR_QUERY.len());
            output.extend_from_slice(&combined[..index]);
            output.extend_from_slice(&combined[index + DSR_QUERY.len()..]);
            self.write_dsr_response();
            return self.defer_output_tail(output, max_len);
        }

        let pending_len = partial_dsr_prefix_len(&combined);
        self.pending.clear();
        self.pending
            .extend_from_slice(&combined[combined.len() - pending_len..]);
        self.defer_output_tail(combined[..combined.len() - pending_len].to_vec(), max_len)
    }

    fn drain_deferred(&mut self, max_len: usize) -> Option<Vec<u8>> {
        if self.deferred.is_empty() {
            return None;
        }
        let len = max_len.min(self.deferred.len());
        Some(self.deferred.drain(..len).collect())
    }

    #[cfg(test)]
    fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.filter_to_len(bytes, usize::MAX)
    }

    #[cfg(test)]
    fn drain_deferred_to_len(&mut self, max_len: usize) -> Vec<u8> {
        self.drain_deferred(max_len).unwrap_or_default()
    }

    fn emit_with_pending(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.pending.is_empty() {
            return bytes.to_vec();
        }
        let mut output = Vec::with_capacity(self.pending.len() + bytes.len());
        output.extend_from_slice(&self.pending);
        output.extend_from_slice(bytes);
        self.pending.clear();
        output
    }

    fn defer_output_tail(&mut self, output: Vec<u8>, max_len: usize) -> Vec<u8> {
        let len = max_len.min(output.len());
        self.deferred.extend(output[len..].iter().copied());
        output[..len].to_vec()
    }

    fn write_dsr_response(&self) {
        if self.input_write_handle.is_null() {
            return;
        }
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

    fn is_done(&self) -> bool {
        self.completed && self.pending.is_empty() && self.deferred.is_empty()
    }
}

fn configured_dsr_bootstrap_timeout() -> Duration {
    let millis = env::var(DSR_BOOTSTRAP_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DSR_BOOTSTRAP_TIMEOUT_MS)
        .clamp(MIN_DSR_BOOTSTRAP_TIMEOUT_MS, MAX_DSR_BOOTSTRAP_TIMEOUT_MS);
    Duration::from_millis(millis)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn partial_dsr_prefix_len(bytes: &[u8]) -> usize {
    (1..DSR_QUERY.len())
        .rev()
        .find(|len| bytes.ends_with(&DSR_QUERY[..*len]))
        .unwrap_or(0)
}

fn sigint_payload_for_child_pid(child_pid: u32, requested_pid: u32) -> Option<&'static [u8]> {
    (child_pid == requested_pid).then_some(CTRL_C_STDIN)
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

#[cfg(test)]
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
    let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
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

fn conpty_flags_without_passthrough() -> ConPtyFlags {
    ConPtyFlags::RESIZE_QUIRK | ConPtyFlags::WIN32_INPUT_MODE
}

fn should_recreate_without_passthrough(flags: ConPtyFlags, error: &io::Error) -> bool {
    flags.uses_passthrough() && error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32)
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

fn create_anonymous_pipe() -> io::Result<(HANDLE, HANDLE)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    if unsafe {
        CreatePipe(
            &mut read,
            &mut write,
            std::ptr::null(),
            CONPTY_READ_BUFFER_SIZE as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok((read, write))
    }
}

/// Create a ConPTY pseudo console of the given size attached to the provided
/// pipes.
fn create_conpty_with_flags(
    cols: u16,
    rows: u16,
    input_read: HANDLE,
    output_write: HANDLE,
    initial_flags: ConPtyFlags,
) -> io::Result<(HPCON, ConPtyFlags)> {
    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };
    let mut hpcon: HPCON = 0;
    let mut flags = initial_flags;
    loop {
        let hr = unsafe {
            CreatePseudoConsole(size, input_read, output_write, flags.bits(), &mut hpcon)
        };
        if hr == S_OK {
            return Ok((hpcon, flags));
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

struct ConPtySpawnState {
    output_read: HANDLE,
    input_write: HANDLE,
    hpcon: HPCON,
    flags: ConPtyFlags,
}

impl ConPtySpawnState {
    fn close(self) {
        unsafe {
            ClosePseudoConsole(self.hpcon);
            CloseHandle(self.input_write);
            CloseHandle(self.output_read);
        }
    }
}

fn create_terminal_conpty(_terminal_id: u32, flags: ConPtyFlags) -> io::Result<ConPtySpawnState> {
    let (output_read, output_write) = create_anonymous_pipe()?;

    let (input_read, input_write) = match create_anonymous_pipe() {
        Ok(pipe) => pipe,
        Err(error) => {
            unsafe {
                CloseHandle(output_read);
                CloseHandle(output_write);
            }
            return Err(error);
        },
    };
    if input_read == INVALID_HANDLE_VALUE || input_write == INVALID_HANDLE_VALUE {
        unsafe {
            CloseHandle(output_read);
            CloseHandle(output_write);
        }
        return Err(io::Error::last_os_error());
    }

    let (hpcon, flags) = match create_conpty_with_flags(80, 24, input_read, output_write, flags) {
        Ok(result) => result,
        Err(e) => {
            unsafe {
                CloseHandle(output_read);
                CloseHandle(output_write);
                CloseHandle(input_read);
                CloseHandle(input_write);
            }
            return Err(e);
        },
    };

    unsafe {
        CloseHandle(input_read);
        CloseHandle(output_write);
    }

    Ok(ConPtySpawnState {
        output_read,
        input_write,
        hpcon,
        flags,
    })
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
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
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

/// Windows PTY backend using native ConPTY with a blocking output reader thread.
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

        let mut conpty = create_terminal_conpty(terminal_id, selected_conpty_flags())
            .with_context(|| err_context(&cmd))?;

        let spawn_result = spawn_child_process(conpty.hpcon, &cmd, terminal_id);
        let (process_handle, thread_handle, child_pid, job_handle) = match spawn_result {
            Ok(result) => result,
            Err(e) if should_recreate_without_passthrough(conpty.flags, &e) => {
                conpty.close();
                conpty = create_terminal_conpty(terminal_id, conpty_flags_without_passthrough())
                    .with_context(|| err_context(&cmd))?;
                match spawn_child_process(conpty.hpcon, &cmd, terminal_id) {
                    Ok(result) => result,
                    Err(e) => {
                        conpty.close();
                        return Err(e).with_context(|| err_context(&cmd));
                    },
                }
            },
            Err(e) => {
                conpty.close();
                return Err(e).with_context(|| err_context(&cmd));
            },
        };

        // Thread handle is not needed after spawn.
        unsafe { CloseHandle(thread_handle) };

        // 6. Store per-terminal state
        self.terminals.lock().unwrap().insert(
            terminal_id,
            Some(ConPtyTerminal {
                hpcon: conpty.hpcon,
                input_write_handle: conpty.input_write,
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

        // 8. Wrap the output read handle in an async reader backed by a blocking thread.
        let owned =
            unsafe { OwnedHandle::from_raw_handle(conpty.output_read as *mut core::ffi::c_void) };
        let dsr_bootstrap =
            command_uses_powershell(&cmd).then_some(DsrBootstrap::new(conpty.input_write));
        let reader =
            Box::new(ConPtyBlockingReader::new(owned, dsr_bootstrap)) as Box<dyn AsyncReader>;

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
        if self.write_sigint_to_child_stdin(pid)? {
            return Ok(());
        }
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
        let terminals = self
            .terminals
            .lock()
            .to_anyhow()
            .with_context(err_context)?;
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

    fn write_sigint_to_child_stdin(&self, pid: u32) -> Result<bool> {
        let err_context = || format!("failed to send SIGINT to pid {}", pid);
        let terminals = self
            .terminals
            .lock()
            .to_anyhow()
            .with_context(err_context)?;
        if let Some((term, payload)) = terminals
            .values()
            .filter_map(|terminal| terminal.as_ref())
            .find_map(|terminal| {
                sigint_payload_for_child_pid(terminal.child_pid, pid)
                    .map(|payload| (terminal, payload))
            })
        {
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    term.input_write_handle,
                    payload.as_ptr(),
                    payload.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                Err(io::Error::last_os_error()).with_context(err_context)
            } else {
                Ok(true)
            }
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

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
        assert!(is_benign_resize_after_exit_hresult(0x80070006_u32 as i32));
        assert!(is_benign_resize_after_exit_hresult(0x8007006d_u32 as i32));
        assert!(!is_benign_resize_after_exit_hresult(0x80070057_u32 as i32));
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

    #[test]
    fn dsr_bootstrap_detects_query_split_across_reads() {
        let mut dsr = DsrBootstrap::new(std::ptr::null_mut());

        assert_eq!(dsr.filter(b"before\x1b["), b"before");
        assert_eq!(dsr.filter(b"6nafter"), b"after");
        assert!(dsr.is_done());
    }

    #[test]
    fn dsr_bootstrap_replays_false_partial_query_prefix() {
        let mut dsr = DsrBootstrap::new(std::ptr::null_mut());

        assert_eq!(dsr.filter(b"before\x1b["), b"before");
        assert_eq!(dsr.filter(b"XXafter"), b"\x1b[XXafter");
        assert!(!dsr.is_done());
    }

    #[test]
    fn dsr_bootstrap_defers_output_that_does_not_fit_read_buffer() {
        let mut dsr = DsrBootstrap::new(std::ptr::null_mut());
        let output = dsr.filter_to_len(b"before\x1b[6nafter", 7);

        assert_eq!(output, b"beforea");
        assert_eq!(dsr.drain_deferred_to_len(16), b"fter");
        assert!(dsr.is_done());
    }

    #[test]
    fn sigint_for_known_child_uses_ctrl_c_stdin_byte() {
        assert_eq!(sigint_payload_for_child_pid(42, 42), Some(&b"\x03"[..]));
        assert_eq!(sigint_payload_for_child_pid(42, 7), None);
    }

    #[test]
    fn passthrough_spawn_invalid_parameter_requests_conpty_recreate() {
        let passthrough = ConPtyFlags::RESIZE_QUIRK
            | ConPtyFlags::WIN32_INPUT_MODE
            | ConPtyFlags::PASSTHROUGH_MODE;
        let no_passthrough = ConPtyFlags::RESIZE_QUIRK | ConPtyFlags::WIN32_INPUT_MODE;

        assert!(should_recreate_without_passthrough(
            passthrough,
            &io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32)
        ));
        assert!(!should_recreate_without_passthrough(
            no_passthrough,
            &io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32)
        ));
        assert!(!should_recreate_without_passthrough(
            passthrough,
            &io::Error::from_raw_os_error(5)
        ));
    }

    #[tokio::test]
    async fn blocking_reader_buffers_chunk_tail() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(b"abcdef".to_vec())).await.expect("send chunk");
        drop(tx);
        let mut reader = ConPtyBlockingReader::from_receiver(rx);
        let mut buf = [0_u8; 2];

        assert_eq!(reader.read(&mut buf).await.expect("first read"), 2);
        assert_eq!(&buf, b"ab");
        assert_eq!(reader.read(&mut buf).await.expect("second read"), 2);
        assert_eq!(&buf, b"cd");
        assert_eq!(reader.read(&mut buf).await.expect("third read"), 2);
        assert_eq!(&buf, b"ef");
        assert_eq!(reader.read(&mut buf).await.expect("eof"), 0);
    }

    #[tokio::test]
    async fn blocking_reader_propagates_read_error() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(Err(io::Error::from_raw_os_error(5)))
            .await
            .expect("send error");
        let mut reader = ConPtyBlockingReader::from_receiver(rx);
        let mut buf = [0_u8; 8];

        let error = reader.read(&mut buf).await.expect_err("read error");

        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn conpty_output_channel_is_bounded() {
        let (tx, _rx) = mpsc::channel::<io::Result<Vec<u8>>>(1);

        tx.try_send(Ok(b"first".to_vec())).expect("first send fits");

        assert!(
            tx.try_send(Ok(b"second".to_vec())).is_err(),
            "bounded ConPTY output channel should apply backpressure when full"
        );
    }

    #[tokio::test]
    async fn blocking_reader_reads_real_conpty_output_until_exit() {
        let marker = "ZELLIJ_BLOCKING_CONPTY_OK";
        let cmd = RunCommand {
            command: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/C".to_owned(),
                "echo".to_owned(),
                marker.to_owned(),
            ],
            ..Default::default()
        };
        let conpty =
            create_terminal_conpty(10_001, conpty_flags_without_passthrough()).expect("ConPTY");
        let (process_handle, thread_handle, _child_pid, job_handle) =
            spawn_child_process(conpty.hpcon, &cmd, 10_001).expect("spawn child");
        unsafe { CloseHandle(thread_handle) };
        let output_read =
            unsafe { OwnedHandle::from_raw_handle(conpty.output_read as *mut core::ffi::c_void) };
        let mut reader = ConPtyBlockingReader::new(output_read, None);
        let mut output = Vec::new();
        let mut buf = [0_u8; 1024];
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for ConPTY output: {}",
                String::from_utf8_lossy(&output)
            );
            match tokio::time::timeout(
                remaining.min(Duration::from_millis(500)),
                reader.read(&mut buf),
            )
            .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(bytes_read)) => {
                    output.extend_from_slice(&buf[..bytes_read]);
                    if String::from_utf8_lossy(&output).contains(marker) {
                        break;
                    }
                },
                Ok(Err(error)) => panic!("ConPTY read failed: {error}"),
                Err(_) => continue,
            }
        }

        unsafe {
            WaitForSingleObject(process_handle, INFINITE);
            ClosePseudoConsole(conpty.hpcon);
            CloseHandle(process_handle);
            CloseHandle(conpty.input_write);
            CloseHandle(job_handle);
        }

        assert!(
            String::from_utf8_lossy(&output).contains(marker),
            "expected marker in ConPTY output, got: {}",
            String::from_utf8_lossy(&output)
        );
    }

    #[tokio::test]
    async fn real_conpty_interactive_cmd_accepts_written_input() {
        let marker = "ZELLIJ_INTERACTIVE_CONPTY_OK";
        let cmd = RunCommand {
            command: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            args: vec!["/D".to_owned(), "/K".to_owned()],
            ..Default::default()
        };
        let (conpty, process_handle, thread_handle, job_handle, mut reader) =
            spawn_test_conpty(&cmd);
        unsafe { CloseHandle(thread_handle) };

        write_to_conpty(conpty.input_write, format!("echo {marker}\r\n").as_bytes());
        let output = read_until_marker(&mut reader, marker, Duration::from_secs(5)).await;

        terminate_job(job_handle).expect("terminate job");
        unsafe {
            WaitForSingleObject(process_handle, INFINITE);
            ClosePseudoConsole(conpty.hpcon);
            CloseHandle(process_handle);
            CloseHandle(conpty.input_write);
            CloseHandle(job_handle);
        }

        assert!(
            String::from_utf8_lossy(&output).contains(marker),
            "expected interactive marker in ConPTY output, got: {}",
            String::from_utf8_lossy(&output)
        );
    }

    #[tokio::test]
    async fn real_conpty_resize_after_child_exit_is_not_fatal() {
        let cmd = RunCommand {
            command: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            args: vec!["/D".to_owned(), "/C".to_owned(), "exit 0".to_owned()],
            ..Default::default()
        };
        let (conpty, process_handle, thread_handle, job_handle, reader) = spawn_test_conpty(&cmd);
        unsafe {
            CloseHandle(thread_handle);
            WaitForSingleObject(process_handle, INFINITE);
        }

        let resize_result = unsafe { ResizePseudoConsole(conpty.hpcon, COORD { X: 90, Y: 25 }) };

        drop(reader);
        unsafe {
            ClosePseudoConsole(conpty.hpcon);
            CloseHandle(process_handle);
            CloseHandle(conpty.input_write);
            CloseHandle(job_handle);
        }
        assert!(
            resize_result == S_OK || is_benign_resize_after_exit_hresult(resize_result),
            "ResizePseudoConsole after child exit returned HRESULT 0x{:08x}",
            resize_result as u32
        );
    }

    #[tokio::test]
    async fn real_conpty_force_kill_reaps_child() {
        let cmd = RunCommand {
            command: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/C".to_owned(),
                "ping -n 30 127.0.0.1 >NUL".to_owned(),
            ],
            ..Default::default()
        };
        let (conpty, process_handle, thread_handle, job_handle, reader) = spawn_test_conpty(&cmd);
        unsafe { CloseHandle(thread_handle) };

        terminate_job(job_handle).expect("terminate job");
        unsafe { WaitForSingleObject(process_handle, INFINITE) };

        let mut exit_code = STILL_ACTIVE as u32;
        let exit_code_ok = unsafe { GetExitCodeProcess(process_handle, &mut exit_code) };

        drop(reader);
        unsafe {
            ClosePseudoConsole(conpty.hpcon);
            CloseHandle(process_handle);
            CloseHandle(conpty.input_write);
            CloseHandle(job_handle);
        }
        assert_ne!(exit_code_ok, 0, "GetExitCodeProcess failed");
        assert_ne!(
            exit_code, STILL_ACTIVE as u32,
            "child process survived Job Object kill"
        );
    }

    #[tokio::test]
    async fn real_conpty_force_kill_reaps_grandchild_process_tree() {
        let marker = "ZELLIJ_GRANDCHILD=";
        let command = concat!(
            "$child = Start-Process -FilePath powershell.exe ",
            "-ArgumentList '-NoLogo','-NoProfile','-Command','Start-Sleep -Seconds 30' ",
            "-WindowStyle Hidden -PassThru; ",
            "Write-Output \"ZELLIJ_GRANDCHILD=$($child.Id)\"; ",
            "Start-Sleep -Seconds 30"
        );
        let cmd = RunCommand {
            command: std::path::PathBuf::from(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            ),
            args: vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-ExecutionPolicy".to_owned(),
                "Bypass".to_owned(),
                "-Command".to_owned(),
                command.to_owned(),
            ],
            ..Default::default()
        };
        let (conpty, process_handle, thread_handle, job_handle, mut reader) =
            spawn_test_conpty(&cmd);
        unsafe { CloseHandle(thread_handle) };
        let output = read_until_marker(&mut reader, marker, Duration::from_secs(5)).await;
        let grandchild_pid = parse_marker_pid(&output, marker).expect("grandchild pid");
        assert!(
            process_is_running(grandchild_pid),
            "grandchild should be running before force kill"
        );

        terminate_job(job_handle).expect("terminate job");
        unsafe { WaitForSingleObject(process_handle, INFINITE) };

        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_running(grandchild_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        let survived = process_is_running(grandchild_pid);
        if survived {
            terminate_process_id(grandchild_pid);
        }

        drop(reader);
        unsafe {
            ClosePseudoConsole(conpty.hpcon);
            CloseHandle(process_handle);
            CloseHandle(conpty.input_write);
            CloseHandle(job_handle);
        }
        assert!(
            !survived,
            "grandchild process {grandchild_pid} survived Job Object kill"
        );
    }

    fn spawn_test_conpty(
        cmd: &RunCommand,
    ) -> (
        ConPtySpawnState,
        HANDLE,
        HANDLE,
        HANDLE,
        ConPtyBlockingReader,
    ) {
        let conpty =
            create_terminal_conpty(10_002, conpty_flags_without_passthrough()).expect("ConPTY");
        let (process_handle, thread_handle, _child_pid, job_handle) =
            spawn_child_process(conpty.hpcon, cmd, 10_002).expect("spawn child");
        let output_read =
            unsafe { OwnedHandle::from_raw_handle(conpty.output_read as *mut core::ffi::c_void) };
        let reader = ConPtyBlockingReader::new(output_read, None);
        (conpty, process_handle, thread_handle, job_handle, reader)
    }

    fn write_to_conpty(input_write: HANDLE, bytes: &[u8]) {
        let mut written = 0;
        let ok = unsafe {
            WriteFile(
                input_write,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "failed to write to ConPTY input");
    }

    async fn read_until_marker(
        reader: &mut ConPtyBlockingReader,
        marker: &str,
        timeout: Duration,
    ) -> Vec<u8> {
        let mut output = Vec::new();
        let mut buf = [0_u8; 1024];
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for ConPTY marker {marker}: {}",
                String::from_utf8_lossy(&output)
            );
            match tokio::time::timeout(
                remaining.min(Duration::from_millis(500)),
                reader.read(&mut buf),
            )
            .await
            {
                Ok(Ok(0)) => return output,
                Ok(Ok(bytes_read)) => {
                    output.extend_from_slice(&buf[..bytes_read]);
                    if String::from_utf8_lossy(&output).contains(marker) {
                        return output;
                    }
                },
                Ok(Err(error)) => panic!("ConPTY read failed: {error}"),
                Err(_) => continue,
            }
        }
    }

    fn parse_marker_pid(bytes: &[u8], marker: &str) -> Option<u32> {
        let output = String::from_utf8_lossy(bytes);
        let start = output.find(marker)? + marker.len();
        let digits = output[start..]
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        digits.parse().ok()
    }

    fn process_is_running(pid: u32) -> bool {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process == std::ptr::null_mut() {
            return false;
        }
        let mut exit_code = 0;
        let ok = unsafe { GetExitCodeProcess(process, &mut exit_code) };
        unsafe { CloseHandle(process) };
        ok != 0 && exit_code == STILL_ACTIVE as u32
    }

    fn terminate_process_id(pid: u32) {
        let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if process != std::ptr::null_mut() {
            unsafe {
                TerminateProcess(process, 1);
                CloseHandle(process);
            }
        }
    }
}
use tokio::sync::mpsc::{self, Receiver};
