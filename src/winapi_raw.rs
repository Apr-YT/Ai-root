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
/// CreateFileW 连接命名管道时，所有实例均忙（服务端尚未 accept）返回此错误码。
/// 客户端标准做法：WaitNamedPipeW 等待实例就绪后重试。
pub const ERROR_PIPE_BUSY: u32 = 231;

// ---- 同步对象等待 ----
pub const INFINITE: u32 = 0xFFFF_FFFF;
pub const WAIT_OBJECT_0: u32 = 0;

/// MultiByteToWideChar 的 CP_OEMCP 特殊值：使用系统 OEM 代码页（中文 Windows 为 GBK 936）
pub const CP_OEMCP: u32 = 1;

// ---- 服务控制管理器（SCM）----
/// OpenSCManagerW 的最小访问权：仅连接（查询/打开服务用）
pub const SC_MANAGER_CONNECT: u32 = 0x0001;
/// OpenServiceW 的完全访问权（含 WRITE_DAC，改服务安全描述符需要）
pub const SERVICE_ALL_ACCESS: u32 = 0x000F_01FF;
/// SetServiceObjectSecurity 的 SECURITY_INFORMATION：只改 DACL（保留所有者的默认 ACL）
pub const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;

// ---- 进程身份（审计：记录管道调用者）----
/// OpenProcess 的受限查询权限（无需管理员即可查询进程身份/路径）
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
/// OpenProcessToken 的查询权限
pub const TOKEN_QUERY: u32 = 0x0008;
/// GetTokenInformation 的 TokenUser 类（取进程主 SID）
pub const TOKEN_USER: u32 = 1;

/// TOKEN_USER 结构（GetTokenInformation 输出，仅取 User.Sid）
#[repr(C)]
pub struct TOKEN_USER {
    pub User: SID_AND_ATTRIBUTES,
}

#[repr(C)]
pub struct SID_AND_ATTRIBUTES {
    pub Sid: *mut c_void,
    pub Attributes: u32,
}

// ---- SCM 失败恢复策略（ChangeServiceConfig2W）----
/// dwInfoLevel：SERVICE_CONFIG_FAILURE_ACTIONS
pub const SERVICE_CONFIG_FAILURE_ACTIONS: u32 = 2;
/// SC_ACTION.Type：重新启动服务
pub const SC_ACTION_RESTART: u32 = 1;

#[repr(C)]
pub struct SC_ACTION {
    pub Type: u32,
    pub Delay: u32,
}

#[repr(C)]
pub struct SERVICE_FAILURE_ACTIONS {
    pub dwResetPeriod: u32,
    pub lpRebootMsg: PCWSTR,
    pub lpCommand: PCWSTR,
    pub cActions: u32,
    pub lpsaActions: *mut SC_ACTION,
}

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
    /// 等待命名管道实例可用（客户端在 CreateFileW 遇 ERROR_PIPE_BUSY 时调用）。
    /// nTimeOut=0 表示永久等待；返回 0 表示超时或管道已消失。
    pub fn WaitNamedPipeW(lpNamedPipeName: PCWSTR, nTimeOut: u32) -> BOOL;

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

    // ---- 服务控制管理器（SCM）----
    pub fn OpenSCManagerW(
        lpMachineName: PCWSTR,
        lpDatabaseName: PCWSTR,
        dwDesiredAccess: u32,
    ) -> HANDLE;

    pub fn OpenServiceW(
        hSCManager: HANDLE,
        lpServiceName: PCWSTR,
        dwDesiredAccess: u32,
    ) -> HANDLE;

    pub fn CloseServiceHandle(hSCObject: HANDLE) -> BOOL;

    /// 设置服务对象安全描述符（DACL）。用于 install 时给当前用户 SID 追加
    /// SERVICE_START(SERVICE_START=0x10=SDDL RP)/SERVICE_STOP(0x20=SDDL WP) 权限，
    /// 使智能体进程（用户身份）无需管理员即可 stop/start 服务 → 自主更新二进制。
    pub fn SetServiceObjectSecurity(
        hService: HANDLE,
        dwSecurityInformation: u32,
        lpSecurityDescriptor: *mut c_void,
    ) -> BOOL;

    /// 多字节（OEM/ANSI 代码页）-> UTF-16。用于正确解码命令输出的本地化文本（如中文）。
    pub fn MultiByteToWideChar(
        CodePage: u32,
        dwFlags: u32,
        lpMultiByteStr: *const u8,
        cbMultiByte: i32,
        lpWideCharStr: *mut u16,
        cchWideChar: i32,
    ) -> i32;

    // ---- 进程身份（审计）----
    /// 获取命名管道客户端的进程 ID（服务端审计"谁在调用"）
    pub fn GetNamedPipeClientProcessId(Pipe: HANDLE, ClientProcessId: *mut u32) -> BOOL;

    pub fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: BOOL, dwProcessId: u32) -> HANDLE;

    pub fn OpenProcessToken(
        ProcessHandle: HANDLE,
        DesiredAccess: u32,
        TokenHandle: *mut HANDLE,
    ) -> BOOL;

    pub fn GetTokenInformation(
        TokenHandle: HANDLE,
        TokenInformationClass: u32,
        TokenInformation: *mut c_void,
        TokenInformationLength: u32,
        ReturnLength: *mut u32,
    ) -> BOOL;

    /// SID -> 账户名（domain\user），用于把客户端进程主 SID 转成可读用户名
    pub fn LookupAccountSidW(
        lpSystemName: PCWSTR,
        Sid: *const c_void,
        Name: *mut u16,
        cchName: *mut u32,
        ReferencedDomainName: *mut u16,
        cchReferencedDomainName: *mut u32,
        peUse: *mut u32,
    ) -> BOOL;

    /// 配置服务失败恢复策略（SCM Recovery Actions，防异常终止后不重启）
    pub fn ChangeServiceConfig2W(hService: HANDLE, dwInfoLevel: u32, lpInfo: *const c_void) -> BOOL;
}
