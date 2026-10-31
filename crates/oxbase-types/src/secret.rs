//! [`Secret`]: a string that never leaks into logs, errors or saved documents.

use std::fmt;

/// A secret string (password, passphrase, token, inline private key PEM).
///
/// It never appears in `Debug` output, has no `Display` and no `Serialize`, so it cannot leak
/// into logs, error messages or saved documents by accident. Read it with [`Secret::expose`] only
/// at the point of use. On drop the buffer is overwritten (best effort, without `unsafe`).
///
/// Equality compares in time independent of where the first difference is (it still depends on
/// the lengths).
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret text. Call only where the value is handed to a library or the network.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Length in bytes (safe to log).
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0.as_bytes(), other.0.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

impl Eq for Secret {}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        // Keep the zeroing store from being optimised away.
        std::hint::black_box(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_and_comparable() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?} {s:#?}"), "Secret(***) Secret(***)");
        assert_eq!(s.expose(), "hunter2");
        assert_eq!(s.len(), 7);
        assert_eq!(s, Secret::from("hunter2"));
        assert_ne!(s, Secret::from("hunter3"));
        assert_ne!(s, Secret::from("hunter22"));
        assert!(Secret::default().is_empty());
    }
}
