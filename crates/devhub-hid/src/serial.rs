//! Windows CDC (USB serial) トランスポート。
//!
//! HM66 等の設定が HID ではなく CDC ACM 経由で行われるデバイス用。
//! kernel32 の CreateFile/ReadFile/WriteFile/SetCommState を直接叩く。

use devhub_core::backend::{HubError, HubResult};
use std::io::{Read, Write};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Dcb {
    dcb_length: u32,
    baud_rate: u32,
    flags: u32,
    w_reserved: u16,
    xon_lim: u16,
    xoff_lim: u16,
    byte_size: u8,
    parity: u8,
    stop_bits: u8,
    xon_char: i8,
    xoff_char: i8,
    error_char: i8,
    eof_char: i8,
    evt_char: i8,
    w_reserved1: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CommTimeouts {
    read_interval_timeout: u32,
    read_total_timeout_multiplier: u32,
    read_total_timeout_constant: u32,
    write_total_timeout_multiplier: u32,
    write_total_timeout_constant: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *mut std::ffi::c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: *mut std::ffi::c_void,
    ) -> isize;
    fn ReadFile(
        hFile: isize,
        lpBuffer: *mut u8,
        nNumberOfBytesToRead: u32,
        lpNumberOfBytesRead: *mut u32,
        lpOverlapped: *mut std::ffi::c_void,
    ) -> i32;
    fn WriteFile(
        hFile: isize,
        lpBuffer: *const u8,
        nNumberOfBytesToWrite: u32,
        lpNumberOfBytesWritten: *mut u32,
        lpOverlapped: *mut std::ffi::c_void,
    ) -> i32;
    fn CloseHandle(hObject: isize) -> i32;
    fn SetCommState(hFile: isize, lpDCB: *mut Dcb) -> i32;
    fn GetCommState(hFile: isize, lpDCB: *mut Dcb) -> i32;
    fn SetCommTimeouts(hFile: isize, lpCommTimeouts: *const CommTimeouts) -> i32;
}

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const INVALID_HANDLE: isize = -1;

pub struct SerialPort {
    handle: isize,
    pub baud: u32,
}

impl SerialPort {
    pub fn open(port: &str, baud: u32) -> HubResult<Self> {
        let path = format!(r"\\.\{}", port);
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE || h == 0 {
            return Err(HubError::Io(format!("open {port}: failed")));
        }
        // DCB
        let mut dcb = Dcb::default();
        dcb.dcb_length = std::mem::size_of::<Dcb>() as u32;
        unsafe {
            if GetCommState(h, &mut dcb) == 0 {
                CloseHandle(h);
                return Err(HubError::Io(format!("GetCommState {port} failed")));
            }
        }
        dcb.baud_rate = baud;
        dcb.byte_size = 8;
        dcb.parity = 0;
        dcb.stop_bits = 0; // 1 stop bit
        // flags: fBinary=1 (bit 0)
        dcb.flags = 1;
        unsafe {
            if SetCommState(h, &mut dcb) == 0 {
                CloseHandle(h);
                return Err(HubError::Io(format!("SetCommState {port} failed")));
            }
            let to = CommTimeouts {
                read_interval_timeout: 50,
                read_total_timeout_multiplier: 10,
                read_total_timeout_constant: 200,
                write_total_timeout_multiplier: 10,
                write_total_timeout_constant: 200,
            };
            if SetCommTimeouts(h, &to) == 0 {
                CloseHandle(h);
                return Err(HubError::Io(format!("SetCommTimeouts {port} failed")));
            }
        }
        Ok(Self { handle: h, baud })
    }

    pub fn read(&mut self, buf: &mut [u8]) -> HubResult<usize> {
        let mut n: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(HubError::Io("serial read failed".into()));
        }
        Ok(n as usize)
    }

    pub fn write_all(&mut self, buf: &[u8]) -> HubResult<()> {
        let mut off = 0;
        while off < buf.len() {
            let mut n: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    buf[off..].as_ptr(),
                    (buf.len() - off) as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || n == 0 {
                return Err(HubError::Io("serial write failed".into()));
            }
            off += n as usize;
        }
        Ok(())
    }
}

impl Drop for SerialPort {
    fn drop(&mut self) {
        if self.handle != 0 && self.handle != INVALID_HANDLE {
            unsafe { CloseHandle(self.handle); }
            self.handle = 0;
        }
    }
}

// Send+Sync: kernel handles are process-global; access is serialized by driver worker.
unsafe impl Send for SerialPort {}
unsafe impl Sync for SerialPort {}
