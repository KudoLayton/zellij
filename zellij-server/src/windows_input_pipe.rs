//! Private overlapped ConPTY input pipe, adapted from Helvesec/rmux 28c4ba6c
//! (crates/rmux-pty/src/backend/windows/io.rs at 1f4571e7).
//! Copyright (c) 2026 The RMUX Authors. MIT: see windows_input.LICENSE.
use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        LocalFree, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, GENERIC_WRITE,
        HANDLE, INVALID_HANDLE_VALUE,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION,
        },
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
        PIPE_ACCESS_INBOUND,
    },
    System::{
        Pipes::{
            CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_TYPE_BYTE, PIPE_WAIT,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

struct LocalAllocation(*mut core::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

fn current_sid() -> io::Result<String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
            null_mut(),
            0,
            &mut size,
        );
    }
    if size < size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::last_os_error());
    }
    // TOKEN_USER contains pointers and requires pointer-aligned storage.
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid_string = null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _allocation = LocalAllocation(sid_string.cast());
    let mut len = 0;
    // The API returns a NUL-terminated SID string allocated by LocalAlloc.
    while unsafe { *sid_string.add(len) } != 0 {
        len += 1;
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(sid_string, len)
    }))
}

struct SameUserSecurity {
    descriptor: LocalAllocation,
}
impl SameUserSecurity {
    fn new() -> io::Result<Self> {
        let sid = current_sid()?;
        let sddl: Vec<u16> = format!("O:{sid}G:{sid}D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor: LocalAllocation(descriptor),
        })
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: 0,
        }
    }
}

pub(super) fn input_pipe(buffer_size: u32) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let security = SameUserSecurity::new()?;
    let mut last_collision = None;
    for _ in 0..8 {
        let name: Vec<u16> = format!(r"\\.\pipe\zellij-conpty-input-{}", uuid::Uuid::new_v4())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        match create_named(&name, buffer_size, &security) {
            Ok(pair) => return Ok(pair),
            Err(error)
                if matches!(
                    error.raw_os_error().map(|e| e as u32),
                    Some(ERROR_ACCESS_DENIED | ERROR_ALREADY_EXISTS | ERROR_PIPE_BUSY)
                ) =>
            {
                last_collision = Some(error)
            },
            Err(error) => return Err(error),
        }
    }
    Err(last_collision
        .unwrap_or_else(|| io::Error::other("ConPTY input pipe allocation exhausted")))
}

fn create_named(
    name: &[u16],
    buffer_size: u32,
    security: &SameUserSecurity,
) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut attributes = security.attributes();
    // ConPTY needs the synchronous server/read end; only our writer overlaps.
    let read = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            buffer_size,
            0,
            &mut attributes,
        )
    };
    if read == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let read = unsafe { OwnedHandle::from_raw_handle(read) };
    let write = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            0,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            null_mut(),
        )
    };
    if write == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let write = unsafe { OwnedHandle::from_raw_handle(write) };
    let mut pid = 0;
    if unsafe { GetNamedPipeClientProcessId(read.as_raw_handle() as HANDLE, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if pid != std::process::id() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unexpected ConPTY input pipe client",
        ));
    }
    Ok((read, write))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pipe_handles_are_not_inheritable() {
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
        let (read, write) = input_pipe(4096).unwrap();
        for handle in [read, write] {
            let mut flags = 0;
            assert_ne!(
                unsafe { GetHandleInformation(handle.as_raw_handle() as HANDLE, &mut flags) },
                0
            );
            assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        }
    }
    #[test]
    fn acl_contains_one_current_user_allow_entry() {
        use windows_sys::Win32::Security::{GetAce, GetSecurityDescriptorDacl, ACCESS_ALLOWED_ACE};
        let security = SameUserSecurity::new().unwrap();
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(
                    security.descriptor.0,
                    &mut present,
                    &mut acl,
                    &mut defaulted,
                )
            },
            0
        );
        assert_ne!(present, 0);
        assert!(!acl.is_null());
        assert_eq!(unsafe { (*acl).AceCount }, 1);
        let mut ace = null_mut();
        assert_ne!(unsafe { GetAce(acl, 0, &mut ace) }, 0);
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        assert_eq!(ace.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE (winnt.h)
        let mut sid = null_mut();
        assert_ne!(
            unsafe {
                ConvertSidToStringSidW((&ace.SidStart as *const u32).cast_mut().cast(), &mut sid)
            },
            0
        );
        let _allocation = LocalAllocation(sid.cast());
        let mut len = 0;
        while unsafe { *sid.add(len) } != 0 {
            len += 1;
        }
        assert_eq!(
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, len) }),
            current_sid().unwrap()
        );
    }
    #[test]
    fn duplicate_named_instance_is_rejected() {
        let security = SameUserSecurity::new().unwrap();
        let name: Vec<u16> = format!(r"\\.\pipe\zellij-test-{}", uuid::Uuid::new_v4())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let _first = create_named(&name, 4096, &security).unwrap();
        assert!(create_named(&name, 4096, &security).is_err());
    }
}
