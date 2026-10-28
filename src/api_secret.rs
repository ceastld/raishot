//! 本机 API 凭据：仅以当前 Windows 用户的 DPAPI 密文落盘，不读取环境变量。
use crate::Result;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    },
};

const PREFIX: &str = "dpapi-v1:";
const ENTROPY: &[u8] = b"rustshot/translation-api-key/v1";
const MAX_SECRET_BYTES: usize = 8 * 1024;

fn encryption_error() -> String {
    tr!(
        "无法加密 API Key，设置未保存；请确认当前 Windows 用户可用后重试。",
        "Could not encrypt the API key; settings were not saved. Check the Windows user and retry."
    )
    .into()
}
fn decryption_error() -> String {
    tr!(
        "无法读取已保存的 API Key，请在翻译设置重新填写；原密文仍保留。",
        "Could not read the saved API key. Enter it again in Translation settings; the original encrypted value is kept."
    )
    .into()
}

struct OwnedBlob(CRYPT_INTEGER_BLOB);
impl Drop for OwnedBlob {
    fn drop(&mut self) {
        if !self.0.pbData.is_null() {
            unsafe {
                // DPAPI 输出可能是明文；复制后清零系统缓冲区再释放。
                std::ptr::write_bytes(self.0.pbData, 0, self.0.cbData as usize);
                let _ = LocalFree(Some(HLOCAL(self.0.pbData.cast())));
            }
        }
    }
}
fn borrowed(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    }
}

pub(crate) fn protect(plain: &str) -> Result<String> {
    if plain.is_empty() {
        return Ok(String::new());
    }
    if plain.len() > MAX_SECRET_BYTES || plain.chars().any(char::is_control) {
        return Err(tr!(
            "API Key 过长或包含控制字符，请重新填写。",
            "The API key is too long or contains control characters. Enter it again."
        )
        .into());
    }
    let input = borrowed(plain.as_bytes());
    let entropy = borrowed(ENTROPY);
    let mut output = OwnedBlob(CRYPT_INTEGER_BLOB::default());
    unsafe {
        // 不使用 LOCAL_MACHINE，密文仅绑定当前用户；禁止 DPAPI 弹窗。
        CryptProtectData(
            &input,
            PCWSTR::null(),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    }
    .map_err(|_| encryption_error())?;
    if output.0.pbData.is_null() || output.0.cbData == 0 {
        return Err(encryption_error().into());
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) };
    let mut encoded = String::with_capacity(PREFIX.len() + bytes.len() * 2);
    encoded.push_str(PREFIX);
    use std::fmt::Write;
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

pub(crate) fn unprotect(encoded: &str) -> Result<String> {
    if encoded.is_empty() {
        return Ok(String::new());
    }
    let hex = encoded
        .strip_prefix(PREFIX)
        .filter(|hex| !hex.is_empty() && hex.len() <= 32 * 1024 && hex.len() % 2 == 0)
        .ok_or_else(decryption_error)?;
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let digit = |value| match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            b'A'..=b'F' => Some(value - b'A' + 10),
            _ => None,
        };
        let high = digit(pair[0]).ok_or_else(decryption_error)?;
        let low = digit(pair[1]).ok_or_else(decryption_error)?;
        bytes.push(high * 16 + low);
    }
    let input = borrowed(&bytes);
    let entropy = borrowed(ENTROPY);
    let mut output = OwnedBlob(CRYPT_INTEGER_BLOB::default());
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    }
    .map_err(|_| decryption_error())?;
    if output.0.pbData.is_null() || output.0.cbData as usize > MAX_SECRET_BYTES {
        return Err(decryption_error().into());
    }
    let plain = unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) };
    let plain = std::str::from_utf8(plain).map_err(|_| decryption_error())?;
    if plain.chars().any(char::is_control) {
        return Err(decryption_error().into());
    }
    Ok(plain.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_dpapi_round_trip_is_encrypted_and_empty_stays_empty() {
        assert_eq!(protect("").unwrap(), "");
        assert_eq!(unprotect("").unwrap(), "");
        let plain = "test-only-placeholder-not-a-real-credential";
        let encrypted = protect(plain).unwrap();
        assert!(encrypted.starts_with(PREFIX));
        assert!(!encrypted.contains(plain));
        assert_eq!(unprotect(&encrypted).unwrap(), plain);
    }

    #[test]
    fn api_key_dpapi_rejects_invalid_or_foreign_envelopes_without_echoing_input() {
        for invalid in ["plain-secret", "dpapi-v1:xyz", "dpapi-v1:0", "dpapi-v1:00"] {
            let error = unprotect(invalid).unwrap_err().to_string();
            assert!(!error.contains(invalid));
        }
        assert!(protect("test\nnot-a-key").is_err());
        assert!(protect(&"x".repeat(MAX_SECRET_BYTES + 1)).is_err());
    }
}
