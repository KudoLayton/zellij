//! Optional side-by-side ConPTY. This does not replace the pipe workers or Job policy.
//! System APIs remain the default; a Console retains its provider until after close.
use std::{
    ffi::{CStr, OsStr, OsString},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HANDLE, HMODULE},
    Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    },
    System::{
        Console::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON},
        LibraryLoader::{
            GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        },
    },
};

use zellij_utils::input::options::ConptyProvider;

type Create = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type Resize = unsafe extern "system" fn(HPCON, COORD) -> i32;
type Close = unsafe extern "system" fn(HPCON);
type Symbol = unsafe extern "system" fn() -> isize;

struct Library(HMODULE);
// The module is immutable and Windows supports calls from multiple threads.
// Its final owner frees it only after all Console owners have closed their HPCONs.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}
impl Library {
    fn symbol(&self, name: &CStr) -> io::Result<Symbol> {
        unsafe { GetProcAddress(self.0, name.as_ptr().cast()) }.ok_or_else(io::Error::last_os_error)
    }
}

pub(super) struct Provider {
    create: Create,
    resize: Resize,
    close: Close,
    description: String,
    // Keep the function pointers valid through every HPCON's close operation.
    library: Option<Library>,
}

impl Provider {
    pub(super) fn system() -> Self {
        Self {
            create: CreatePseudoConsole,
            resize: ResizePseudoConsole,
            close: ClosePseudoConsole,
            description: "system (kernel32)".into(),
            library: None,
        }
    }

    pub(super) fn from_config(configured: Option<ConptyProvider>) -> Arc<Self> {
        Arc::new(Self::select(
            Self::resolve_mode(configured, std::env::var_os("ZELLIJ_CONPTY_PROVIDER")),
            std::env::var_os("ZELLIJ_CONPTY_DIR"),
            std::env::current_exe(),
        ))
    }

    fn resolve_mode(
        configured: Option<ConptyProvider>,
        override_mode: Option<OsString>,
    ) -> Option<OsString> {
        // Keep process-scoped diagnostic/rollback overrides; ordinary users only
        // need conpty_provider in config.kdl and the adjacent installed pair.
        override_mode.or_else(|| configured.map(|provider| provider.as_str().into()))
    }

    fn select(
        mode: Option<OsString>,
        directory: Option<OsString>,
        exe: io::Result<PathBuf>,
    ) -> Self {
        // A directory alone must never opt the user into loading executable code.
        if mode.as_deref().is_none() || mode.as_deref() == Some(OsStr::new("system")) {
            return Self::system();
        }
        let result = if mode.as_deref() == Some(OsStr::new("bundled")) {
            let directory = match directory {
                Some(path) => Ok(PathBuf::from(path)),
                None => exe.and_then(|exe| {
                    exe.parent()
                        .map(|parent| parent.join("conpty").join(std::env::consts::ARCH))
                        .ok_or_else(|| invalid("executable has no parent directory"))
                }),
            };
            directory.and_then(|directory| Self::load(&directory))
        } else {
            Err(invalid("ZELLIJ_CONPTY_PROVIDER must be system or bundled"))
        };
        match result {
            Ok(provider) => provider,
            Err(error) => {
                log::warn!("Optional ConPTY unavailable: {error}; using system ConPTY (graphics passthrough is not guaranteed)");
                Self::system()
            },
        }
    }

    fn load(directory: &Path) -> io::Result<Self> {
        if !directory.is_absolute() {
            return Err(invalid(
                "ZELLIJ_CONPTY_DIR must be an absolute trusted directory",
            ));
        }
        let directory = directory.canonicalize()?;
        let dll = directory.join("conpty.dll").canonicalize()?;
        let host = directory.join("OpenConsole.exe").canonicalize()?;
        if dll.parent() != Some(directory.as_path()) || host.parent() != Some(directory.as_path()) {
            return Err(invalid(
                "ConPTY binaries must reside together, not link outside their directory",
            ));
        }
        let machine = target_machine()?;
        let dll_version = binary_info(&dll, machine)?;
        let host_version = binary_info(&host, machine)?;
        validate_versions(dll_version, host_version)?;
        let path = wide(dll.as_os_str())?;
        // Never search the CWD or PATH for this DLL or its dependencies.
        let module = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return Err(io::Error::last_os_error());
        }
        let library = Library(module);
        let description = format!(
            "bundled {} ({}.{}.{}.{})",
            dll.display(),
            dll_version.0 >> 16,
            dll_version.0 & 0xffff,
            dll_version.1 >> 16,
            dll_version.1 & 0xffff,
        );
        // The named exports have these signatures in Microsoft's conpty.h.
        // Missing exports drop the library before falling back to system APIs.
        let (create, resize, close) = unsafe {
            (
                std::mem::transmute::<Symbol, Create>(
                    library.symbol(c"ConptyCreatePseudoConsole")?,
                ),
                std::mem::transmute::<Symbol, Resize>(
                    library.symbol(c"ConptyResizePseudoConsole")?,
                ),
                std::mem::transmute::<Symbol, Close>(library.symbol(c"ConptyClosePseudoConsole")?),
            )
        };
        Ok(Self {
            create,
            resize,
            close,
            description,
            library: Some(library),
        })
    }

    pub(super) fn is_bundled(&self) -> bool {
        self.library.is_some()
    }

    pub(super) fn description(&self) -> &str {
        &self.description
    }

    pub(super) fn effective_flags(&self, system_flags: u32) -> u32 {
        // Preserve the existing system policy, but do not transfer undocumented
        // legacy bits to a different provider. 0 is the standard ConPTY mode.
        if self.is_bundled() {
            0
        } else {
            system_flags
        }
    }

    pub(super) unsafe fn create(
        &self,
        size: COORD,
        input: HANDLE,
        output: HANDLE,
        flags: u32,
        hpcon: *mut HPCON,
    ) -> i32 {
        (self.create)(size, input, output, flags, hpcon)
    }
    pub(super) unsafe fn resize(&self, hpcon: HPCON, size: COORD) -> i32 {
        (self.resize)(hpcon, size)
    }
    pub(super) unsafe fn close(&self, hpcon: HPCON) {
        (self.close)(hpcon)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut result: Vec<u16> = value.encode_wide().collect();
    if result.contains(&0) {
        return Err(invalid("NUL in ConPTY path"));
    }
    result.push(0);
    Ok(result)
}

fn target_machine() -> io::Result<u16> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(0x8664),
        "aarch64" => Ok(0xaa64),
        "x86" => Ok(0x14c),
        _ => Err(invalid("unsupported bundled ConPTY architecture")),
    }
}

fn pe_machine(reader: &mut (impl Read + Seek)) -> io::Result<u16> {
    let mut dos = [0; 64];
    reader.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(invalid("ConPTY binary has no DOS header"));
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap());
    reader.seek(SeekFrom::Start(offset as u64))?;
    let mut pe = [0; 6];
    reader.read_exact(&mut pe)?;
    if &pe[..4] != b"PE\0\0" {
        return Err(invalid("ConPTY binary has no PE header"));
    }
    Ok(u16::from_le_bytes([pe[4], pe[5]]))
}

fn binary_info(path: &Path, expected_machine: u16) -> io::Result<(u32, u32)> {
    if pe_machine(&mut File::open(path)?)? != expected_machine {
        return Err(invalid(
            "ConPTY binary architecture does not match this Zellij executable",
        ));
    }
    let path = wide(path.as_os_str())?;
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut()) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    // Aligned storage for the version APIs; the queried value is copied before drop.
    let mut data = vec![0u32; (size as usize).div_ceil(4)];
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut value = std::ptr::null_mut();
    let mut len = 0;
    let root = [b'\\' as u16, 0];
    if unsafe { VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut value, &mut len) } == 0
        || value.is_null()
        || (len as usize) < size_of::<VS_FIXEDFILEINFO>()
    {
        return Err(invalid("missing fixed ConPTY file version"));
    }
    let version = unsafe { std::ptr::read_unaligned(value.cast::<VS_FIXEDFILEINFO>()) };
    if version.dwSignature != 0xfeef04bd {
        return Err(invalid("invalid ConPTY version signature"));
    }
    Ok((version.dwFileVersionMS, version.dwFileVersionLS))
}

fn validate_versions(dll: (u32, u32), host: (u32, u32)) -> io::Result<()> {
    if dll == (0, 0) || dll != host {
        return Err(invalid(
            "conpty.dll and OpenConsole.exe must have the same nonzero file version",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_override_precedes_configuration() {
        assert_eq!(Provider::resolve_mode(None, None), None);
        for configured in [ConptyProvider::System, ConptyProvider::Bundled] {
            assert_eq!(
                Provider::resolve_mode(Some(configured), None),
                Some(configured.as_str().into())
            );
            for override_mode in ["system", "bundled", "invalid"] {
                assert_eq!(
                    Provider::resolve_mode(Some(configured), Some(override_mode.into())),
                    Some(override_mode.into())
                );
            }
        }
    }

    #[test]
    fn system_is_default_even_with_directory_override() {
        let missing_exe = || Err(io::Error::other("unused"));
        for mode in [None, Some("system".into())] {
            let provider = Provider::select(mode, Some("untrusted-relative".into()), missing_exe());
            assert!(!provider.is_bundled());
            assert_eq!(provider.effective_flags(14), 14);
            assert_eq!(provider.effective_flags(6), 6);
        }
    }

    #[test]
    fn invalid_selection_and_missing_binaries_fall_back_without_loading() {
        let dir = tempfile::tempdir().unwrap();
        for (mode, path) in [
            ("unknown", dir.path().as_os_str()),
            ("bundled", OsStr::new("relative")),
            ("bundled", dir.path().as_os_str()),
        ] {
            let provider = Provider::select(
                Some(mode.into()),
                Some(path.into()),
                std::env::current_exe(),
            );
            assert!(!provider.is_bundled());
        }
        assert!(Provider::load(Path::new("relative")).is_err());
    }

    #[test]
    #[ignore = "requires a trusted matching pair in absolute ZELLIJ_CONPTY_DIR"]
    fn bundled_validation_failures_fall_back() {
        let source =
            PathBuf::from(std::env::var_os("ZELLIJ_CONPTY_DIR").expect("provider fixture"));
        assert!(Provider::load(&source).unwrap().is_bundled());
        let dir = tempfile::tempdir().unwrap();
        let dll = dir.path().join("conpty.dll");
        let host = dir.path().join("OpenConsole.exe");
        let assert_fallback = || {
            assert!(!Provider::select(
                Some("bundled".into()),
                Some(dir.path().as_os_str().into()),
                std::env::current_exe(),
            )
            .is_bundled());
        };
        std::fs::copy(source.join("conpty.dll"), &dll).unwrap();
        assert_eq!(
            Provider::load(dir.path()).err().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        assert_fallback();

        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
        let version_dll = system.join("version.dll");
        let machine = target_machine().unwrap();
        // This explicit fixture test needs a provider version different from
        // the OS DLL. Do not silently skip a failed mismatch setup.
        assert_ne!(
            binary_info(&dll, machine).unwrap(),
            binary_info(&version_dll, machine).unwrap()
        );
        std::fs::copy(&version_dll, &host).unwrap();
        assert!(Provider::load(dir.path())
            .err()
            .unwrap()
            .to_string()
            .contains("same nonzero file version"));
        assert_fallback();

        std::fs::copy(source.join("OpenConsole.exe"), &host).unwrap();
        let mut bytes = std::fs::read(&dll).unwrap();
        let pe = u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize;
        bytes[pe + 4..pe + 6].copy_from_slice(&0u16.to_le_bytes());
        std::fs::write(&dll, bytes).unwrap();
        assert!(Provider::load(dir.path())
            .err()
            .unwrap()
            .to_string()
            .contains("architecture"));
        assert_fallback();

        // A genuine OS DLL supplies valid, matching PE metadata but none of
        // the ConPTY exports. No generated/untrusted executable code is loaded.
        std::fs::copy(&version_dll, &dll).unwrap();
        std::fs::copy(&version_dll, &host).unwrap();
        assert_eq!(
            Provider::load(dir.path()).err().unwrap().raw_os_error(),
            Some(127)
        );
        assert_fallback();
    }

    #[test]
    fn rejects_mismatched_or_missing_versions() {
        assert!(validate_versions((1, 2), (1, 2)).is_ok());
        assert!(validate_versions((1, 2), (1, 3)).is_err());
        assert!(validate_versions((0, 0), (0, 0)).is_err());
    }

    #[test]
    fn pe_header_validation_and_architecture_check() {
        let mut data = vec![0; 70];
        data[..2].copy_from_slice(b"MZ");
        data[60..64].copy_from_slice(&64u32.to_le_bytes());
        data[64..68].copy_from_slice(b"PE\0\0");
        data[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        assert_eq!(pe_machine(&mut io::Cursor::new(&data)).unwrap(), 0x8664);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrong-machine.dll");
        std::fs::write(&path, &data).unwrap();
        assert!(binary_info(&path, 0xaa64)
            .unwrap_err()
            .to_string()
            .contains("architecture"));
        data[64] = 0;
        assert!(pe_machine(&mut io::Cursor::new(&data)).is_err());
        assert!(pe_machine(&mut io::Cursor::new(b"MZ")).is_err());
    }
}
