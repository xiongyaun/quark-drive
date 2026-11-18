//! Offline request serialization recovered from APK `hn0.a.b(TreeMap)`.
//!
//! This module does not send requests or implement SecurityGuard signing/encryption.
//! Provenance: REA evidence p2-transport-b, APK SHA-256
//! e32e25409ff5d278bfce2f264af03e1e2d495c503ca5673c391955a390c7eeb7.
use std::collections::BTreeMap;

use crate::{Error, Result};

/// Exact unsigned parameter map after the Android context has been added.
/// Values may contain secrets; deliberately has no Debug implementation.
pub struct NativeSmsParameters(BTreeMap<String, String>);

impl NativeSmsParameters {
    /// Context values must come from observed native configuration, not web defaults.
    /// Only ASCII parameter names have been observed; reject unsupported ordering.
    pub fn new(fields: BTreeMap<String, String>) -> Result<Self> {
        if fields.is_empty()
            || fields.keys().any(|key| {
                key.is_empty()
                    || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    || key == "sign"
            })
        {
            return Err(Error::Other("无效的原生短信未签名参数".into()));
        }
        Ok(Self(fields))
    }

    /// Java TreeMap order, concatenated key=value with no separators or escaping.
    /// The platform signer adds its own suffix/prefix; this is only its input.
    pub fn signing_input(&self) -> String {
        let mut output = String::new();
        for (key, value) in &self.0 {
            output.push_str(key);
            output.push('=');
            output.push_str(value);
        }
        output
    }

    /// Form plaintext before the APK's encryption/framing stage.
    /// An unavailable signer must fail before network I/O; never use an empty sign.
    pub fn signed_form(&self, signature: &str) -> Result<Vec<u8>> {
        if signature.is_empty() {
            return Err(Error::Other("原生短信签名不可用，禁止发送请求".into()));
        }
        let mut fields = self.0.clone();
        fields.insert("sign".into(), signature.into());
        Ok(fields
            .iter()
            .map(|(key, value)| format!("{key}={}", java_form_value(value)))
            .collect::<Vec<_>>()
            .join("&")
            .into_bytes())
    }
}

// java.net.URLEncoder uses '+' for space and escapes '~' (unlike RFC3986).
fn java_form_value(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => {
                output.push(byte as char)
            }
            b' ' => output.push('+'),
            _ => {
                output.push('%');
                output.push(HEX[(byte >> 4) as usize] as char);
                output.push(HEX[(byte & 15) as usize] as char);
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_signing_and_encoded_form_are_different_stages() -> Result<()> {
        let params = NativeSmsParameters::new(BTreeMap::from([
            ("z".into(), "中文 +~*".into()),
            ("a".into(), "x&y=1".into()),
        ]))?;
        assert_eq!(params.signing_input(), "a=x&y=1z=中文 +~*");
        assert_eq!(
            params.signed_form("test+signature")?,
            b"a=x%26y%3D1&sign=test%2Bsignature&z=%E4%B8%AD%E6%96%87+%2B%7E*"
        );
        Ok(())
    }

    #[test]
    fn unavailable_signer_and_unsupported_keys_are_rejected() -> Result<()> {
        let params = NativeSmsParameters::new(BTreeMap::from([("mobile".into(), "test".into())]))?;
        assert!(params.signed_form("").is_err());
        for key in ["sign", "", "a&b", "键"] {
            assert!(
                NativeSmsParameters::new(BTreeMap::from([(key.into(), "test".into())])).is_err()
            );
        }
        Ok(())
    }
}
