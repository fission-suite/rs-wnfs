use std::fmt::Debug;

use super::error::HamtError;
use crate::{HashOutput, HASH_BYTE_SIZE};
use anyhow::{bail, Result};
use sha3::{Digest, Sha3_256};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const MAX_CURSOR_DEPTH: usize = HASH_BYTE_SIZE * 2;

//--------------------------------------------------------------------------------------------------
// Type Definition
//--------------------------------------------------------------------------------------------------

/// A common trait for the ability to generate a hash of some data.
///
/// # Examples
///
/// ```
/// use sha3::{Digest, Sha3_256};
/// use wnfs::{Hasher, HashOutput};
///
/// struct MyHasher;
///
/// impl Hasher for MyHasher {
///     fn hash<D: AsRef<[u8]>>(data: &D) -> HashOutput {
///         let mut hasher = Sha3_256::new();
///         hasher.update(data.as_ref());
///         hasher.finalize().into()
///     }
/// }
/// ```
pub trait Hasher {
    /// Generates a hash of the given data.
    fn hash<D: AsRef<[u8]>>(data: &D) -> HashOutput;
}

/// HashNibbles is a wrapper around a byte slice that provides a cursor for traversing the nibbles.
#[derive(Clone)]
pub(crate) struct HashNibbles<'a> {
    pub digest: &'a HashOutput,
    cursor: usize,
}

/// TODO(appcypher): Add docs.
#[derive(Clone, Default)]
pub struct HashKey {
    pub digest: HashOutput,
    length: u8,
}

/// TODO(appcypher): Add docs.
#[derive(Clone)]
pub struct HashKeyIterator<'a> {
    pub hash_key: &'a HashKey,
    cursor: u8,
}

//--------------------------------------------------------------------------------------------------
// Implementation
//--------------------------------------------------------------------------------------------------

impl<'a> HashNibbles<'a> {
    /// Creates a new `HashNibbles` instance from a `[u8; 32]` hash.
    pub(crate) fn new(digest: &'a HashOutput) -> HashNibbles<'a> {
        Self::with_cursor(digest, 0)
    }

    /// Constructs a `HashNibbles` with custom cursor index.
    pub(crate) fn with_cursor(digest: &'a HashOutput, cursor: usize) -> HashNibbles<'a> {
        Self { digest, cursor }
    }

    /// Gets the next nibble from the hash.
    pub(crate) fn try_next(&mut self) -> Result<usize> {
        if let Some(nibble) = self.next() {
            return Ok(nibble as usize);
        }
        bail!(HamtError::CursorOutOfBounds)
    }

    /// Gets the current cursor position.
    #[inline]
    pub(crate) fn get_cursor(&self) -> usize {
        self.cursor
    }
}

impl Iterator for HashNibbles<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= MAX_CURSOR_DEPTH {
            return None;
        }

        let byte = self.digest[self.cursor / 2];
        let byte = if self.cursor % 2 == 0 {
            byte >> 4
        } else {
            byte & 0b0000_1111
        };

        self.cursor += 1;
        Some(byte)
    }
}

impl Debug for HashNibbles<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut nibbles_str = String::new();
        for nibble in HashNibbles::with_cursor(self.digest, 0) {
            nibbles_str.push_str(&format!("{nibble:1X}"));
        }

        f.debug_struct("HashNibbles")
            .field("hash", &nibbles_str)
            .field("cursor", &self.cursor)
            .finish()
    }
}

impl Hasher for Sha3_256 {
    fn hash<D: AsRef<[u8]>>(data: &D) -> HashOutput {
        let mut hasher = Self::default();
        hasher.update(data.as_ref());
        hasher.finalize().into()
    }
}

impl HashKey {
    /// Creates a new `HashKey` instance from a `[u8; 32]` hash.
    pub(crate) fn with_length(digest: HashOutput, length: u8) -> HashKey {
        Self { digest, length }
    }

    /// Pushes a nibble to the end of the hash.
    pub fn push(&mut self, nibble: u8) {
        let offset = self.length as usize / 2;
        let byte = self.digest[offset];
        let byte = if self.length as usize % 2 == 0 {
            nibble << 4
        } else {
            byte | (nibble & 0x0F)
        };

        self.digest[offset] = byte;
        self.length += 1;
    }

    #[inline(always)]
    /// Gets the length of the hash.
    /// TODO(appcypher): Add examples.
    pub fn len(&self) -> usize {
        self.length as usize
    }

    /// Checks if the hash is empty.
    /// TODO(appcypher): Add examples.
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Get the nibble at specified offset.
    /// TODO(appcypher): Add examples.
    pub fn get(&self, index: u8) -> Option<u8> {
        if index >= self.length {
            return None;
        }

        let byte = self.digest.get(index as usize / 2)?;
        Some(if index % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0F
        })
    }

    /// Creates an iterator over the nibbles of the hash.
    /// TODO(appcypher): Add examples.
    pub fn iter(&self) -> HashKeyIterator {
        HashKeyIterator {
            hash_key: self,
            cursor: 0,
        }
    }
}

impl Debug for HashKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x")?;
        for nibble in self.iter() {
            write!(f, "{nibble:1X}")?;
        }

        Ok(())
    }
}

impl PartialEq for HashKey {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl Iterator for HashKeyIterator<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.hash_key.length {
            return None;
        }

        let byte = self.hash_key.get(self.cursor)?;
        self.cursor += 1;
        Some(byte)
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_nibbles_can_cursor_over_digest() {
        let key = {
            let mut bytes = [0u8; HASH_BYTE_SIZE];
            bytes[0] = 0b1000_1100;
            bytes[1] = 0b1010_1010;
            bytes[2] = 0b1011_1111;
            bytes[3] = 0b1111_1101;
            bytes
        };

        let hashnibbles = &mut HashNibbles::new(&key);
        let expected_nibbles = [
            0b1000, 0b1100, 0b1010, 0b1010, 0b1011, 0b1111, 0b1111, 0b1101,
        ];

        for (got, expected) in hashnibbles.zip(expected_nibbles.into_iter()) {
            assert_eq!(expected, got);
        }

        // Exhaust the iterator.
        let _ = hashnibbles
            .take(MAX_CURSOR_DEPTH - expected_nibbles.len())
            .collect::<Vec<_>>();

        assert_eq!(hashnibbles.next(), None);
    }

    #[test]
    fn can_push_and_get_nibbles_from_hashkey() {
        let mut hashkey = HashKey::default();
        for i in 0..HASH_BYTE_SIZE {
            hashkey.push((i % 16) as u8);
            hashkey.push((15 - i % 16) as u8);
        }

        for i in 0..HASH_BYTE_SIZE {
            assert_eq!(hashkey.get(i as u8 * 2).unwrap(), (i % 16) as u8);
            assert_eq!(hashkey.get(i as u8 * 2 + 1).unwrap(), (15 - i % 16) as u8);
        }
    }
}
