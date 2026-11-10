//! 库内密钥的加解密。
//!
//! 只认两个解密钥来源：`ACB_KEY_FILE`（0600 文件，优先）与 `ACB_KEY`（env）。
//! 值可以是 64 位 hex 或 base64 编码的 32 字节；密文格式 `v1:<kid>:<base64>`。
//! 密钥本身不进库、不进配置文件；缺密钥时遇到密文必须报错，不能当明文读。

use std::fs;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};

const PREFIX: &str = "v1";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

pub struct AcbKey {
    key: LessSafeKey,
    kid: String,
}

impl std::fmt::Debug for AcbKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcbKey")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl AcbKey {
    /// 从显式 32 字节材料构造；`kid` 取 SHA-256 前 8 字节的 hex。
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Result<Self, String> {
        let unbound = UnboundKey::new(&AES_256_GCM, &bytes)
            .map_err(|_| "invalid AES-256-GCM key".to_owned())?;
        let digest = digest(&SHA256, &bytes);
        let kid = digest.as_ref()[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            key: LessSafeKey::new(unbound),
            kid,
        })
    }

    /// 读取当前进程可用的密钥。两个来源都没有时返回 `Ok(None)`。
    pub fn load() -> Result<Option<Self>, String> {
        if let Some(path) = std::env::var_os("ACB_KEY_FILE") {
            let path = Path::new(&path);
            check_file_mode(path)?;
            let text = fs::read_to_string(path)
                .map_err(|error| format!("read ACB_KEY_FILE {}: {error}", path.display()))?;
            return parse_material(text.trim()).map(Some);
        }
        if let Ok(text) = std::env::var("ACB_KEY") {
            let text = text.trim();
            if !text.is_empty() {
                return parse_material(text).map(Some);
            }
        }
        Ok(None)
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// 加密成 `v1:<kid>:<base64(nonce||ciphertext)>`。
    pub fn encrypt(&self, plaintext: &str) -> Result<String, String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "cannot generate nonce".to_owned())?;
        let nonce_bytes = nonce;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = plaintext.as_bytes().to_vec();
        self.key
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| "encrypt secret failed".to_owned())?;
        let mut payload = Vec::with_capacity(NONCE_LEN + in_out.len());
        payload.extend_from_slice(&nonce_bytes);
        payload.extend_from_slice(&in_out);
        Ok(format!("{PREFIX}:{}:{}", self.kid, BASE64.encode(payload)))
    }

    /// 解密 `v1:<kid>:<...>`；kid 不匹配或格式不对都返回错误。
    pub fn decrypt(&self, text: &str) -> Result<String, String> {
        let mut parts = text.splitn(3, ':');
        let Some(prefix) = parts.next() else {
            return Err("secret is not encrypted".to_owned());
        };
        if prefix != PREFIX {
            return Err(format!("unsupported secret prefix {prefix:?}"));
        }
        let Some(kid) = parts.next() else {
            return Err("encrypted secret has no key id".to_owned());
        };
        if kid != self.kid {
            return Err(format!(
                "encrypted secret uses key {kid}, current key is {}",
                self.kid
            ));
        }
        let Some(payload) = parts.next() else {
            return Err("encrypted secret has no payload".to_owned());
        };
        let mut bytes = BASE64
            .decode(payload)
            .map_err(|error| format!("decode encrypted secret: {error}"))?;
        if bytes.len() < NONCE_LEN {
            return Err("encrypted secret payload is too short".to_owned());
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[..NONCE_LEN]);
        let mut in_out = bytes.split_off(NONCE_LEN);
        let plaintext = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut in_out,
            )
            .map_err(|_| "decrypt secret failed (wrong key or corrupted value)".to_owned())?;
        String::from_utf8(plaintext.to_vec())
            .map_err(|error| format!("decrypted secret is not utf-8: {error}"))
    }
}

pub fn is_encrypted(value: &str) -> bool {
    value.starts_with("v1:")
}

fn parse_material(text: &str) -> Result<AcbKey, String> {
    let text = text.trim();
    let bytes = if text.len() == KEY_LEN * 2 && text.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut bytes = [0u8; KEY_LEN];
        for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
            let hex = std::str::from_utf8(chunk).map_err(|_| "invalid hex key".to_owned())?;
            bytes[index] = u8::from_str_radix(hex, 16).map_err(|_| "invalid hex key".to_owned())?;
        }
        bytes
    } else {
        let decoded = BASE64
            .decode(text)
            .map_err(|error| format!("ACB_KEY must be 64 hex or base64 32 bytes: {error}"))?;
        if decoded.len() != KEY_LEN {
            return Err(format!(
                "ACB_KEY decodes to {} bytes, expected {KEY_LEN}",
                decoded.len()
            ));
        }
        let mut bytes = [0u8; KEY_LEN];
        bytes.copy_from_slice(&decoded);
        bytes
    };
    AcbKey::from_bytes(bytes)
}

#[cfg(unix)]
fn check_file_mode(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path)
        .map_err(|error| format!("stat ACB_KEY_FILE {}: {error}", path.display()))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "ACB_KEY_FILE {} has mode {mode:o}; use 0600",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_file_mode(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_key() {
        let key = AcbKey::from_bytes([7u8; KEY_LEN]).unwrap();
        let sealed = key.encrypt("hello").unwrap();
        assert!(sealed.starts_with("v1:"));
        assert_eq!(key.decrypt(&sealed).unwrap(), "hello");

        let other = AcbKey::from_bytes([8u8; KEY_LEN]).unwrap();
        assert!(other.decrypt(&sealed).is_err());
    }

    #[test]
    fn rejects_malformed_values() {
        let key = AcbKey::from_bytes([1u8; KEY_LEN]).unwrap();
        assert!(key.decrypt("plain").is_err());
        assert!(key.decrypt("v1:nope:AAAA").is_err());
        assert!(key.decrypt("v1::AAAA").is_err());
    }

    #[test]
    fn accepts_hex_and_base64_material() {
        let bytes = [3u8; KEY_LEN];
        let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let base64 = BASE64.encode(bytes);
        assert_eq!(
            parse_material(&hex).unwrap().kid(),
            parse_material(&base64).unwrap().kid()
        );
    }
}
