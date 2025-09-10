pub mod cli;
pub mod consts;
pub mod data;
pub mod envs;
pub mod errors;
pub mod home;
pub mod input;
pub mod kdl;
pub mod pane_size;
pub mod plugin_api;
pub mod position;
pub mod session_serialization;
pub mod setup;
pub mod shared;

#[cfg(windows)]
pub mod windows_utils;

// The following modules can't be used when targeting wasm
#[cfg(not(target_family = "wasm"))]
pub mod channels; // Requires async_std
#[cfg(not(target_family = "wasm"))]
pub mod downloader; // Requires async_std
#[cfg(not(target_family = "wasm"))]
pub mod ipc; // Requires interprocess
#[cfg(not(target_family = "wasm"))]
pub mod logging; // Requires log4rs

#[cfg(not(target_family = "wasm"))]
pub use ::{
    anyhow, async_channel, async_std, clap, common_path, humantime, interprocess, lazy_static,
    miette, notify_debouncer_full, regex, serde, signal_hook, surf, tempfile, termwiz, vte,
};

pub use ::prost;

#[cfg(target_family = "unix")]
pub use ::{libc, nix};

#[cfg(windows)]
pub fn is_socket(file: &std::fs::DirEntry) -> std::io::Result<bool> {
    use std::ffi::{OsStr, OsString};
    fn convert_path(pipe_name: &OsStr, hostname: Option<&OsStr>) -> Vec<u16> {
        static PREFIX_LITERAL: &str = r"\\";
        static PIPEFS_LITERAL: &str = r"\pipe\zellij\";

        let hostname = hostname.unwrap_or_else(|| OsStr::new("."));

        let mut path = OsString::with_capacity(
            PREFIX_LITERAL.len() + hostname.len() + PIPEFS_LITERAL.len() + pipe_name.len(),
        );
        path.push(PREFIX_LITERAL);
        path.push(hostname);
        path.push(PIPEFS_LITERAL);
        path.push(pipe_name);

        let mut path = path.encode_wide().collect::<Vec<u16>>();
        path.push(0); // encode_wide does not include the terminating NULL, so we have to add it ourselves
        path
    }

    use std::os::windows::ffi::OsStrExt;
    use winapi::um::namedpipeapi::WaitNamedPipeW;

    let path = convert_path(file.path().file_name().unwrap(), None);
    let check_result = unsafe { WaitNamedPipeW(path.as_ptr(), 1) };
    if check_result == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(true)
}

#[cfg(unix)]
pub fn is_socket(file: &std::fs::DirEntry) -> std::io::Result<bool> {
    use std::os::unix::fs::FileTypeExt;
    Ok(file.file_type()?.is_socket())
}
