//! 最小化 Win32 原生绑定（不依赖 windows-sys 的特性门控）
//!
//! 仅声明中继所需的少数 API 与常量，使用稳定的 Win32 值。
//! 类型统一用 `*mut c_void`，避免与外部 crate 版本耦合。
//! 参考：https://learn.microsoft.com/windows/win32/api/ 各 API 文档

#![allow(non_snake_case, non_camel_case_types)]

use core::ffi::c_void;

pub type HANDLE = *mut c_void;
pub type BOOL = i32;
pub type PCWSTR = *const u16;

pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

/// SECURITY_ATTRIBUTES 结构（用于管道安全描述符）
#[repr(C)]
pub struct SECURITY_ATTRIBUTES {
    pub nLength: u32,
    pub lpSecurityDescriptor: *mut c_void,
    pub bInheritHandle: BOOL,
}

// ---- 命名管道 ----
pub const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
pub const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
pub const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
pub const PIPE_WAIT: u32 = 0x0000_0000;
pub const PIPE_UNLIMITED_INSTANCES: u32 = 255;

// ---- 文件访问 ----
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const OPEN_EXISTING: u32 = 3;
pub const FILE_SHARE_READ: u32 = 0x0000_0001;
pub const FILE_SHARE_WRITE: u32 = 0x0000_0002;

/// ConnectNamedPipe 在客户端已抢先连接时返回此错误码（视为已连接）
pub const ERROR_PIPE_CONNECTED: u32 = 535;

// ---- 同步对象等待 ----
pub const INFINITE: u32 = 0xFFFF_FFFF;
pub const WAIT_OBJECT_0: u32 = 0;

/// MultiByteToWideChar 的 CP_OEMCP 特殊值：使用系统 OEM 代码页（中文 Windows 为 GBK 936）
pub const CP_OEMCP: u32 = 1;

extern "system" {
    pub fn CreateNamedPipeW(
        lpName: PCWSTR,
        dwOpenMode: u32,
        dwPipeMode: u32,
        nMaxInstances: u32,
        nOutBufferSize: u32,
        nInBufferSize: u32,
        nDefaultTimeOut: u32,
        lpSecurityAttributes: *const SECURITY_ATTRIBUTES,
    ) -> HANDLE;

    pub fn ConnectNamedPipe(hNamedPipe: HANDLE, lpOverlapped: *mut c_void) -> BOOL;

    pub fn CreateFileW(
        lpFileName: PCWSTR,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *const SECURITY_ATTRIBUTES,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: HANDLE,
    ) -> HANDLE;

    pub fn CloseHandle(hObject: HANDLE) -> BOOL;

    /// 取消指定句柄上所有进行中的 I/O（用于在不关闭管道句柄的前提下，
    /// 中断阻塞中的 ConnectNamedPipe，避免监控线程 CloseHandle 引发 use-after-free）。
    pub fn CancelIoEx(hObject: HANDLE, lpOverlapped: *mut c_void) -> BOOL;

    pub fn CreateEventW(
        lpEventAttributes: *const SECURITY_ATTRIBUTES,
        bManualReset: BOOL,
        bInitialState: BOOL,
        lpName: PCWSTR,
    ) -> HANDLE;

    pub fn SetEvent(hEvent: HANDLE) -> BOOL;

    /// 等待单个对象；dwMilliseconds=INFINITE 时无限阻塞，=0 时立即返回当前信号状态
    pub fn WaitForSingleObject(hHandle: HANDLE, dwMilliseconds: u32) -> u32;

    pub fn GetLastError() -> u32;

    pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        StringSecurityDescriptor: PCWSTR,
        StringSDRevision: u32,
        SecurityDescriptor: *mut *mut c_void,
        SecurityDescriptorSize: *mut u32,
    ) -> BOOL;

    pub fn LocalFree(hMem: *mut c_void) -> *mut c_void;

    /// 多字节（OEM/ANSI 代码页）-> UTF-16。用于正确解码命令输出的本地化文本（如中文）。
    pub fn MultiByteToWideChar(
        CodePage: u32,
        dwFlags: u32,
        lpMultiByteStr: *const u8,
        cbMultiByte: i32,
        lpWideCharStr: *mut u16,
        cchWideChar: i32,
    ) -> i32;
}
