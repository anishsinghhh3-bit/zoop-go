use std::{mem::size_of, os::windows::ffi::OsStrExt, path::Path, ptr, thread, time::Duration};
use windows_sys::Win32::{
    Foundation::{GlobalFree, HGLOBAL, HWND, POINT},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
    },
    UI::Shell::DROPFILES,
};

const FILE_DROP_FORMAT: u32 = 15;

struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

struct ClipboardMemory(HGLOBAL);

impl Drop for ClipboardMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GlobalFree(self.0) };
        }
    }
}

pub fn copy_file(owner: isize, path: &Path) -> Result<(), String> {
    let names: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let header_size = size_of::<DROPFILES>();
    let mut memory = ClipboardMemory(unsafe {
        GlobalAlloc(GMEM_MOVEABLE, header_size + names.len() * size_of::<u16>())
    });
    if memory.0.is_null() {
        return Err("Windows could not allocate clipboard memory.".into());
    }

    unsafe {
        let data = GlobalLock(memory.0).cast::<u8>();
        if data.is_null() {
            return Err("Windows could not prepare the file for the clipboard.".into());
        }
        data.cast::<DROPFILES>().write(DROPFILES {
            pFiles: header_size as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: 0,
            fWide: 1,
        });
        ptr::copy_nonoverlapping(
            names.as_ptr(),
            data.add(header_size).cast::<u16>(),
            names.len(),
        );
        GlobalUnlock(memory.0);
    }

    // Clipboard ownership can be briefly held by another app or a clipboard manager.
    let _clipboard = (0..10)
        .find_map(|attempt| {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(25));
            }
            (unsafe { OpenClipboard(owner as HWND) } != 0).then_some(ClipboardGuard)
        })
        .ok_or("Another app is using the clipboard. Try Copy file again.")?;

    unsafe {
        if EmptyClipboard() == 0 || SetClipboardData(FILE_DROP_FORMAT, memory.0).is_null() {
            return Err("Windows could not copy the file. Try Copy file again.".into());
        }
    }
    // Windows owns the movable allocation after SetClipboardData succeeds.
    memory.0 = ptr::null_mut();
    Ok(())
}
