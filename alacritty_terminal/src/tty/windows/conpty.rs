use std::io::Error;
use std::os::windows::io::IntoRawHandle;
use std::{i16, mem, ptr};

use mio_anonymous_pipes::{EventedAnonRead, EventedAnonWrite};
use winapi::shared::basetsd::{PSIZE_T, SIZE_T};
use winapi::shared::minwindef::{BYTE, DWORD};
use winapi::shared::ntdef::{HANDLE, HRESULT, LPWSTR};
use winapi::shared::winerror::S_OK;
use winapi::um::libloaderapi::{GetModuleHandleA, GetProcAddress, LoadLibraryA};
use winapi::um::processthreadsapi::{
    CreateProcessW, InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
    PROCESS_INFORMATION, STARTUPINFOW,
};
use winapi::um::winbase::{EXTENDED_STARTUPINFO_PRESENT, STARTF_USESTDHANDLES, STARTUPINFOEXW};
use winapi::um::wincontypes::{COORD, HPCON};

use crate::config::PtyConfig;
use crate::event::OnResize;
use crate::grid::Dimensions;
use crate::term::SizeInfo;
use crate::tty::windows::child::ChildExitWatcher;
use crate::tty::windows::{cmdline, win32_string, Pty};

/// Dynamically load the pseudoconsole API from either kernel32.dll
/// or conpty.dll, depending on if the latter exists or not. The conpty.dll
/// from the Windows Terminal project supports loading OpenConsole.exe, which
/// has has many improvements and bugfixes compared to the standard conpty that
/// ships with Windows.
///
/// Just copy the conpty.dll and OpenConsole.exe files to the same
/// directory as the Alacritty.exe to use it.
///
/// The field names are deliberately PascalCase as this matches
/// the defined symbols in kernel32 and also is the convention
/// that the `winapi` crate follows.
#[allow(non_snake_case)]
struct ConptyApi {
    CreatePseudoConsole:
        unsafe extern "system" fn(COORD, HANDLE, HANDLE, DWORD, *mut HPCON) -> HRESULT,
    ResizePseudoConsole: unsafe extern "system" fn(HPCON, COORD) -> HRESULT,
    ClosePseudoConsole: unsafe extern "system" fn(HPCON),
}

impl ConptyApi {
    /// Load the API or None if it cannot be found.
    pub fn new() -> Option<Self> {
        // Unsafe because windows API calls.
        unsafe {
            let mut hmodule = LoadLibraryA("conpty.dll\0".as_ptr() as _);
            if hmodule.is_null() {
                // Otherwise just use the standard conpty from kernel32.
                hmodule = GetModuleHandleA("kernel32\0".as_ptr() as _);
            }
            assert!(!hmodule.is_null());

            let cpc = GetProcAddress(hmodule, "CreatePseudoConsole\0".as_ptr() as _);
            let rpc = GetProcAddress(hmodule, "ResizePseudoConsole\0".as_ptr() as _);
            let clpc = GetProcAddress(hmodule, "ClosePseudoConsole\0".as_ptr() as _);

            if cpc.is_null() || rpc.is_null() || clpc.is_null() {
                None
            } else {
                Some(Self {
                    CreatePseudoConsole: mem::transmute(cpc),
                    ResizePseudoConsole: mem::transmute(rpc),
                    ClosePseudoConsole: mem::transmute(clpc),
                })
            }
        }
    }
}

/// RAII Pseudoconsole.
pub struct Conpty {
    pub handle: HPCON,
    api: ConptyApi,
}

impl Drop for Conpty {
    fn drop(&mut self) {
        // XXX: This will block until the conout pipe is drained. Will cause a deadlock if the
        // conout pipe has already been dropped by this point.
        //
        // See PR #3084 and https://docs.microsoft.com/en-us/windows/console/closepseudoconsole.
        unsafe { (self.api.ClosePseudoConsole)(self.handle) }
    }
}

// The ConPTY handle can be sent between threads.
unsafe impl Send for Conpty {}

pub fn new(config: &PtyConfig, size: &SizeInfo) -> Option<Pty> {
    let mut pty_handle = 0 as HPCON;

    let api = ConptyApi::new()?;

    // Passing 0 as the size parameter allows the "system default" buffer
    // size to be used. There may be small performance and memory advantages
    // to be gained by tuning this in the future, but it's likely a reasonable
    // start point.
    let (conout, conout_pty_handle) = miow::pipe::anonymous(0).unwrap();
    let (conin_pty_handle, conin) = miow::pipe::anonymous(0).unwrap();

    let coord =
        coord_from_sizeinfo(size).expect("Overflow when creating initial size on pseudoconsole");

    // Create the Pseudo Console, using the pipes.
    let result = unsafe {
        (api.CreatePseudoConsole)(
            coord,
            conin_pty_handle.into_raw_handle(),
            conout_pty_handle.into_raw_handle(),
            0,
            &mut pty_handle as *mut HPCON,
        )
    };

    assert_eq!(result, S_OK);

    let mut success;

    // Prepare child process startup info.

    let mut size: SIZE_T = 0;

    let mut startup_info_ex: STARTUPINFOEXW = Default::default();

    startup_info_ex.StartupInfo.lpTitle = std::ptr::null_mut() as LPWSTR;

    startup_info_ex.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;

    // Setting this flag but leaving all the handles as default (null) ensures the
    // PTY process does not inherit any handles from this Alacritty process.
    startup_info_ex.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;

    // Create the appropriately sized thread attribute list.
    unsafe {
        let failure =
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size as PSIZE_T) > 0;

        // This call was expected to return false.
        if failure {
            panic_shell_spawn();
        }
    }

    let mut attr_list: Box<[BYTE]> = vec![0; size].into_boxed_slice();

    // Set startup info's attribute list & initialize it
    //
    // Lint failure is spurious; it's because winapi's definition of PROC_THREAD_ATTRIBUTE_LIST
    // implies it is one pointer in size (32 or 64 bits) but really this is just a dummy value.
    // Casting a *mut u8 (pointer to 8 bit type) might therefore not be aligned correctly in
    // the compiler's eyes.
    #[allow(clippy::cast_ptr_alignment)]
    {
        startup_info_ex.lpAttributeList = attr_list.as_mut_ptr() as _;
    }

    unsafe {
        success = InitializeProcThreadAttributeList(
            startup_info_ex.lpAttributeList,
            1,
            0,
            &mut size as PSIZE_T,
        ) > 0;

        if !success {
            panic_shell_spawn();
        }
    }

    // Set thread attribute list's Pseudo Console to the specified ConPTY.
    unsafe {
        success = UpdateProcThreadAttribute(
            startup_info_ex.lpAttributeList,
            0,
            22 | 0x0002_0000, // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE.
            pty_handle,
            mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null_mut(),
        ) > 0;

        if !success {
            panic_shell_spawn();
        }
    }

    let cmdline = win32_string(&cmdline(config));
    let cwd = config.working_directory.as_ref().map(win32_string);

    let mut proc_info: PROCESS_INFORMATION = Default::default();
    unsafe {
        success = CreateProcessW(
            ptr::null(),
            cmdline.as_ptr() as LPWSTR,
            ptr::null_mut(),
            ptr::null_mut(),
            false as i32,
            EXTENDED_STARTUPINFO_PRESENT,
            ptr::null_mut(),
            cwd.as_ref().map_or_else(ptr::null, |s| s.as_ptr()),
            &mut startup_info_ex.StartupInfo as *mut STARTUPINFOW,
            &mut proc_info as *mut PROCESS_INFORMATION,
        ) > 0;

        if !success {
            panic_shell_spawn();
        }
    }

    let conin = EventedAnonWrite::new(conin);
    let conout = EventedAnonRead::new(conout);

    let child_watcher = ChildExitWatcher::new(proc_info.hProcess).unwrap();
    let conpty = Conpty { handle: pty_handle, api };

    Some(Pty::new(conpty, conout, conin, child_watcher))
}

// Panic with the last os error as message.
fn panic_shell_spawn() {
    panic!("Unable to spawn shell: {}", Error::last_os_error());
}

impl OnResize for Conpty {
    fn on_resize(&mut self, sizeinfo: &SizeInfo) {
        if let Some(coord) = coord_from_sizeinfo(sizeinfo) {
            let result = unsafe { (self.api.ResizePseudoConsole)(self.handle, coord) };
            assert_eq!(result, S_OK);
        }
    }
}

/// Helper to build a COORD from a SizeInfo, returning None in overflow cases.
fn coord_from_sizeinfo(size: &SizeInfo) -> Option<COORD> {
    let lines = size.screen_lines();
    let columns = size.columns();

    if columns <= i16::MAX as usize && lines <= i16::MAX as usize {
        Some(COORD { X: columns as i16, Y: lines as i16 })
    } else {
        None
    }
}
