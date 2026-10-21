//! Authenticode 签名验证（更新信任根）
//!
//! ## 为什么必须有这一层
//!
//! 改造前 `service::update_self()` 的安全防护只有两条：PE 头校验 + 把 SHA-256
//! 打印到 stdout。这在安全上是零 —— 攻击者当然能构造一个带正确 MZ/PE 头的恶意
//! 二进制，而打印指纹没有任何比对动作。结果是「任何能执行 `AI-Root update`
//! 的人都能把任意代码变成 LocalSystem 服务」。
//!
//! 本模块提供两道判定：
//!
//! 1. **签名完整性与信任链**：`WinVerifyTrust` + `WINTRUST_ACTION_GENERIC_VERIFY_V2`。
//!    这保证文件被签名、且签名后未被篡改（哈希覆盖整个 PE）。
//! 2. **发布者绑定**：取签名者证书的 DER 编码做 SHA-256，与策略文件
//!    `trusted_publishers` 白名单比对。**只验「有签名」不够** —— 任何人都能买
//!    或自签一张证书，必须绑定到具体发布者。
//!
//! 两者**同时通过**才允许更新。任一步失败、或信任根为空，一律拒绝更新
//! （安全失败，而不是「先装上再说」）。
//!
//! ## 实现取舍
//!
//! - 用 `WinVerifyTrust` 而不是自己解析 PKCS#7：签名验证涉及证书链、吊销、
//!   时间戳等大量细节，自研必然出错。
//! - 证书指纹自己算（`sha2`）而不是猜 `CertGetCertificateContextProperty` 的
//!   属性 ID：常量猜错会静默返回错误指纹，属「看起来在工作但其实没验证」的
//!   典型失败模式。
//! - 关闭吊销联网检查（`WTD_REVOCATION_CHECK_NONE` + `WTD_CACHE_ONLY_URL_RETRIEVAL`）：
//!   服务是 SYSTEM 且可能在无网环境启动，联网检查会让更新长时间挂起。
//!   代价是不检查吊销列表 —— 对「个人机器 + 固定发布者白名单」的场景可接受，
//!   已在文档的「已知限制」中列明。

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use crate::common;
use crate::winapi_raw;

/// 签名验证报告（所有失败都表达为字段，便于 CLI 完整展示）
#[derive(Debug, Clone)]
pub struct SignatureReport {
    pub path: PathBuf,
    /// Windows 信任链验证是否通过（签名完整 + 证书链可信）
    pub chain_trusted: bool,
    /// 失败时的 Win32/HRESULT 码（十六进制），便于排障
    pub trust_error: Option<String>,
    /// 签名者证书 SHA-256 指纹（大写 hex，无分隔符）
    pub signer_thumbprint: Option<String>,
    /// 签名者显示名（便于人眼确认是不是预期的发布者）
    pub signer_subject: Option<String>,
    /// 指纹是否命中策略中的 trusted_publishers
    pub publisher_allowed: bool,
    /// 文件自身 SHA-256（写审计，便于比对发布物）
    pub file_sha256: Option<String>,
}

impl SignatureReport {
    /// 是否满足「可以安装为服务」的全部条件
    pub fn is_acceptable(&self) -> bool {
        self.chain_trusted && self.publisher_allowed && self.signer_thumbprint.is_some()
    }
}

/// 计算文件 SHA-256（流式，避免把大文件读进内存）
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect())
}

/// 验证文件的 Authenticode 签名，并与可信发布者白名单比对。
pub fn verify_file(path: &Path, trusted_publishers: &[String]) -> SignatureReport {
    let mut report = SignatureReport {
        path: path.to_path_buf(),
        chain_trusted: false,
        trust_error: None,
        signer_thumbprint: None,
        signer_subject: None,
        publisher_allowed: false,
        file_sha256: sha256_file(path).ok(),
    };

    if !path.is_file() {
        report.trust_error = Some("文件不存在".into());
        return report;
    }

    let (ok, err) = win_verify_trust(path);
    report.chain_trusted = ok;
    report.trust_error = err;

    // 即使信任链失败也尝试取签名者，便于向用户解释「是谁签的但不受信」
    if let Some((thumb, subject)) = extract_signer(path) {
        report.publisher_allowed = trusted_publishers.iter().any(|p| {
            p.replace([' ', ':', '-'], "")
                .eq_ignore_ascii_case(&thumb.replace([' ', ':', '-'], ""))
        });
        report.signer_thumbprint = Some(thumb);
        report.signer_subject = subject;
    }
    report
}

// ============================================================
// WinVerifyTrust
// ============================================================

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// WINTRUST_ACTION_GENERIC_VERIFY_V2 = {00AAC56B-CD44-11d0-8CC2-00C04FC295EE}
const ACTION_GENERIC_VERIFY_V2: Guid = Guid {
    data1: 0x00AA_C56B,
    data2: 0xCD44,
    data3: 0x11D0,
    data4: [0x8C, 0xC2, 0x00, 0xC0, 0x4F, 0xC2, 0x95, 0xEE],
};

#[repr(C)]
struct WintrustFileInfo {
    cb_struct: u32,
    file_path: *const u16,
    file: winapi_raw::HANDLE,
    known_subject: *const c_void,
}

#[repr(C)]
struct WintrustData {
    cb_struct: u32,
    policy_callback_data: *mut c_void,
    sip_client_data: *mut c_void,
    ui_choice: u32,
    revocation_checks: u32,
    union_choice: u32,
    file: *mut WintrustFileInfo,
    state_action: u32,
    state_data: winapi_raw::HANDLE,
    url_reference: *mut u16,
    prov_flags: u32,
    ui_context: u32,
    signature_settings: *mut c_void,
}

/// 执行 WinVerifyTrust。返回 `(是否通过, 失败码)`
fn win_verify_trust(path: &Path) -> (bool, Option<String>) {
    const WTD_UI_NONE: u32 = 2;
    const WTD_REVOKE_NONE: u32 = 0;
    const WTD_CHOICE_FILE: u32 = 1;
    const WTD_STATEACTION_VERIFY: u32 = 1;
    const WTD_STATEACTION_CLOSE: u32 = 2;
    // WTD_REVOCATION_CHECK_NONE(0x10) | WTD_CACHE_ONLY_URL_RETRIEVAL(0x1000)
    const WTD_PROV_FLAGS: u32 = 0x10 | 0x1000;

    let wide = common::to_wide(&path.to_string_lossy());
    let mut file_info = WintrustFileInfo {
        cb_struct: std::mem::size_of::<WintrustFileInfo>() as u32,
        file_path: wide.as_ptr(),
        file: std::ptr::null_mut(),
        known_subject: std::ptr::null(),
    };
    let mut data = WintrustData {
        cb_struct: std::mem::size_of::<WintrustData>() as u32,
        policy_callback_data: std::ptr::null_mut(),
        sip_client_data: std::ptr::null_mut(),
        ui_choice: WTD_UI_NONE,
        revocation_checks: WTD_REVOKE_NONE,
        union_choice: WTD_CHOICE_FILE,
        file: &mut file_info,
        state_action: WTD_STATEACTION_VERIFY,
        state_data: std::ptr::null_mut(),
        url_reference: std::ptr::null_mut(),
        prov_flags: WTD_PROV_FLAGS,
        ui_context: 0,
        signature_settings: std::ptr::null_mut(),
    };
    let mut action = Guid {
        data1: ACTION_GENERIC_VERIFY_V2.data1,
        data2: ACTION_GENERIC_VERIFY_V2.data2,
        data3: ACTION_GENERIC_VERIFY_V2.data3,
        data4: ACTION_GENERIC_VERIFY_V2.data4,
    };

    let status = unsafe {
        winapi_raw::WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action as *mut Guid as *mut c_void,
            &mut data as *mut WintrustData as *mut c_void,
        )
    };

    // 必须显式 CLOSE，否则 WinVerifyTrust 内部状态（含临时文件/句柄）不会被释放
    data.state_action = WTD_STATEACTION_CLOSE;
    unsafe {
        winapi_raw::WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action as *mut Guid as *mut c_void,
            &mut data as *mut WintrustData as *mut c_void,
        );
    }

    if status == 0 {
        (true, None)
    } else {
        (false, Some(format!("0x{:08X}", status as u32)))
    }
}

// ============================================================
// 提取签名者证书
// ============================================================

/// CERT_CONTEXT 的最小布局（只需读取 `pbCertEncoded` / `cbCertEncoded`）
#[repr(C)]
struct CertContext {
    encoding_type: u32,
    cert_encoded: *const u8,
    cert_encoded_len: u32,
    cert_info: *const c_void,
    cert_store: winapi_raw::HANDLE,
}

/// 提取签名者证书指纹（SHA-256 of DER）与显示名。
fn extract_signer(path: &Path) -> Option<(String, Option<String>)> {
    use std::ptr;

    const CERT_QUERY_OBJECT_FILE: u32 = 1;
    const CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED: u32 = 1 << 10;
    const CERT_QUERY_FORMAT_FLAG_BINARY: u32 = 2;
    const CMSG_SIGNER_CERT_INFO_PARAM: u32 = 7;
    const CERT_FIND_SUBJECT_CERT: u32 = 0x000B_0000;
    const X509_ASN_ENCODING: u32 = 0x0000_0001;
    const PKCS_7_ASN_ENCODING: u32 = 0x0001_0000;
    const CERT_NAME_SIMPLE_DISPLAY_TYPE: u32 = 4;

    let wide = common::to_wide(&path.to_string_lossy());
    let mut encoding: u32 = 0;
    let mut content: u32 = 0;
    let mut format: u32 = 0;
    let mut store: winapi_raw::HANDLE = ptr::null_mut();
    let mut msg: winapi_raw::HANDLE = ptr::null_mut();

    let ok = unsafe {
        winapi_raw::CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            wide.as_ptr() as *const c_void,
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            &mut encoding,
            &mut content,
            &mut format,
            &mut store,
            &mut msg,
            ptr::null_mut(),
        )
    };
    if ok == 0 || msg.is_null() || store.is_null() {
        return None;
    }

    // 签名者 CERT_INFO 的长度
    let mut info_len: u32 = 0;
    unsafe {
        winapi_raw::CryptMsgGetParam(msg, CMSG_SIGNER_CERT_INFO_PARAM, 0, ptr::null_mut(), &mut info_len);
    }
    if info_len == 0 {
        unsafe {
            winapi_raw::CryptMsgClose(msg);
            winapi_raw::CertCloseStore(store, 0);
        }
        return None;
    }
    let mut info_buf = vec![0u8; info_len as usize];
    let got = unsafe {
        winapi_raw::CryptMsgGetParam(
            msg,
            CMSG_SIGNER_CERT_INFO_PARAM,
            0,
            info_buf.as_mut_ptr() as *mut c_void,
            &mut info_len,
        )
    };
    if got == 0 {
        unsafe {
            winapi_raw::CryptMsgClose(msg);
            winapi_raw::CertCloseStore(store, 0);
        }
        return None;
    }

    // 在证书库里按 CERT_INFO 找签名者证书
    let cert = unsafe {
        winapi_raw::CertFindCertificateInStore(
            store,
            X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
            0,
            CERT_FIND_SUBJECT_CERT,
            info_buf.as_ptr() as *const c_void,
            ptr::null_mut(),
        )
    };
    if cert.is_null() {
        unsafe {
            winapi_raw::CryptMsgClose(msg);
            winapi_raw::CertCloseStore(store, 0);
        }
        return None;
    }

    // 证书 DER → SHA-256
    let ctx = unsafe { &*(cert as *const CertContext) };
    let thumb = if !ctx.cert_encoded.is_null() && ctx.cert_encoded_len > 0 {
        use sha2::{Digest, Sha256};
        let der = unsafe {
            std::slice::from_raw_parts(ctx.cert_encoded, ctx.cert_encoded_len as usize)
        };
        let mut h = Sha256::new();
        h.update(der);
        Some(
            h.finalize()
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<String>(),
        )
    } else {
        None
    };

    // 显示名（失败不影响指纹）
    let mut name_buf = [0u16; 256];
    let name_len = unsafe {
        winapi_raw::CertGetNameStringW(
            cert as *const c_void,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            ptr::null_mut(),
            name_buf.as_mut_ptr(),
            name_buf.len() as u32,
        )
    };
    let subject = if name_len > 1 {
        Some(String::from_utf16_lossy(
            &name_buf[..(name_len as usize - 1).min(name_buf.len())],
        ))
    } else {
        None
    };

    unsafe {
        winapi_raw::CertFreeCertificateContext(cert);
        winapi_raw::CryptMsgClose(msg);
        winapi_raw::CertCloseStore(store, 0);
    }

    thumb.map(|t| (t, subject))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // 空文件 SHA-256 是公开固定值：e3b0c442...b855
        let dir = std::env::temp_dir().join(format!("airoot-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("empty.bin");
        std::fs::write(&p, b"").unwrap();
        let h = sha256_file(&p).unwrap();
        assert_eq!(
            h,
            "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsigned_temp_file_is_not_acceptable() {
        // 未签名的普通文件必须判为不可接受（信任链失败）
        let dir = std::env::temp_dir().join(format!("airoot-sig2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plain.bin");
        std::fs::write(&p, b"not signed at all").unwrap();

        let r = verify_file(&p, &[]);
        assert!(!r.chain_trusted, "未签名文件不应通过信任链验证");
        assert!(!r.is_acceptable(), "未签名文件不得被接受为更新包");
        assert!(r.trust_error.is_some(), "应给出失败码");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 本机上找一个**嵌入**签名（Authenticode）的 PE 作为测试样本。
    ///
    /// 为什么不能直接用 `System32\*.exe`：Windows 系统二进制绝大多数是
    /// **catalog 签名**（签名不嵌在文件里，而在 `.cat` 目录文件中），
    /// `WinVerifyTrust` 的 GENERIC_VERIFY_V2 默认**不查 catalog**，
    /// 因此那些文件会如实返回 `TRUST_E_NOSIGNATURE`。见下方专门测试。
    ///
    /// 找不到样本时返回 None —— 调用方必须**跳过并如实说明**，不得假装通过。
    fn embedded_signed_sample() -> Option<std::path::PathBuf> {
        // 不写死任何本机路径（仓库对外公开，不能留私有目录痕迹）：
        // 按 PATH 逐个查找常见被签名的可执行文件，并**确认它确实带嵌入签名**
        // 才作为样本返回 —— 否则后续用例会在「样本其实没签名」的假前提上断言。
        const NAMES: &[&str] = &["python.exe", "node.exe", "git.exe", "cargo.exe"];
        let path = std::env::var_os("PATH")?;
        for name in NAMES {
            for dir in std::env::split_paths(&path) {
                let cand = dir.join(name);
                if !cand.is_file() {
                    continue;
                }
                if verify_file(&cand, &[]).chain_trusted {
                    return Some(cand);
                }
            }
        }
        None
    }

    #[test]
    fn empty_trust_list_never_accepts_any_signature() {
        // 即使文件真被签名，信任根为空时也必须拒绝 —— 这是「安全失败」的核心断言
        let Some(sample) = embedded_signed_sample() else {
            eprintln!("[skip] 本机无嵌入签名样本，未验证（不谎报通过）");
            return;
        };
        let r = verify_file(&sample, &[]);
        assert!(
            !r.is_acceptable(),
            "trusted_publishers 为空时不得接受任何签名（即使 {sample:?} 有有效签名）"
        );
    }

    #[test]
    fn catalog_signed_system_binary_is_not_accepted() {
        // 明确记录并验证「只接受嵌入签名」这一设计选择：
        // System32 下的系统文件是 catalog 签名，GENERIC_VERIFY_V2 看不到它。
        let sys = Path::new(r"C:\Windows\System32\notepad.exe");
        if !sys.is_file() {
            return;
        }
        let r = verify_file(sys, &[]);
        assert!(
            !r.chain_trusted,
            "catalog 签名的系统二进制不应通过嵌入签名验证（这是预期行为：\
             本实现只接受 Authenticode 嵌入签名）"
        );
        assert!(!r.is_acceptable());
    }

    #[test]
    fn signed_binary_exposes_thumbprint() {
        // 真实嵌入签名 PE：能取到稳定指纹（只读，无副作用）
        let Some(sample) = embedded_signed_sample() else {
            eprintln!("[skip] 本机无嵌入签名样本，未验证（不谎报通过）");
            return;
        };
        let r = verify_file(&sample, &[]);
        assert!(
            r.chain_trusted,
            "嵌入签名的二进制应通过信任链验证: {:?} (file={sample:?})",
            r.trust_error
        );
        let thumb = r.signer_thumbprint.expect("应能提取签名者证书指纹");
        assert_eq!(thumb.len(), 64, "SHA-256 指纹应为 64 个 hex 字符: {thumb}");
        // 把它加入信任列表后应被接受（验证白名单比对逻辑真的生效）
        let r2 = verify_file(&sample, &[thumb.clone()]);
        assert!(r2.publisher_allowed, "指纹应命中白名单");
        assert!(r2.is_acceptable(), "签名可信 + 发布者在白名单 → 应可接受");
    }

    #[test]
    fn thumbprint_with_separators_still_matches() {
        let Some(sample) = embedded_signed_sample() else {
            eprintln!("[skip] 本机无嵌入签名样本，未验证（不谎报通过）");
            return;
        };
        let r = verify_file(&sample, &[]);
        let Some(thumb) = r.signer_thumbprint else {
            return;
        };
        // 用户从 certutil 复制来的指纹常带空格
        let spaced: String = thumb
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let r2 = verify_file(&sample, &[spaced]);
        assert!(r2.publisher_allowed, "带空格的指纹应同样匹配");
    }
}
