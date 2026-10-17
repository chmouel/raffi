//! Wrappers for passwords, passphrases and session keys.
//!
//! Neither type implements `Display`, and `Debug` prints `<redacted>`, so a
//! secret cannot end up in logs or in the derived `Debug` of a message by
//! accident. Callers reach the plaintext only through `expose`.

use std::fmt;

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Raw output of a command that may contain secrets.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(value: Vec<u8>) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The output as text, without one trailing newline.
    pub fn into_secret_line(self) -> Option<Secret> {
        let mut text = String::from_utf8(self.0).ok()?;
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        Some(Secret::new(text))
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let secret = Secret::new("hunter2".into());
        let bytes = SecretBytes::new(b"hunter2\n".to_vec());
        assert_eq!(format!("{secret:?} {bytes:?}"), "<redacted> <redacted>");
        assert_eq!(bytes.into_secret_line().unwrap().expose(), "hunter2");
    }
}
