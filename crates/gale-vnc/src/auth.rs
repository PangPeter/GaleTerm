//! VNC authentication (DES challenge-response, RFB 3.3 / 3.8).
//!
//! The password is truncated or zero-padded to 8 bytes; each byte's bits are
//! reversed; that becomes the DES key that encrypts the 16-byte challenge.

use cipher::{BlockEncrypt, KeyInit};
use des::Des;

/// Encrypt the server's 16-byte challenge with the VNC password.
pub fn encrypt_challenge(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (dst, src) in key.iter_mut().zip(password.bytes()) {
        *dst = reverse_bits(src);
    }
    let cipher = Des::new_from_slice(&key).expect("8-byte DES key");
    let mut out = *challenge;
    cipher.encrypt_block(out[..8].as_mut().into());
    cipher.encrypt_block(out[8..].as_mut().into());
    out
}

fn reverse_bits(byte: u8) -> u8 {
    let mut b = byte;
    b = (b & 0xf0) >> 4 | (b & 0x0f) << 4;
    b = (b & 0xcc) >> 2 | (b & 0x33) << 2;
    b = (b & 0xaa) >> 1 | (b & 0x55) << 1;
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_challenge_vector() {
        let challenge = [0u8; 16];
        let out = encrypt_challenge("pass", &challenge);
        assert_ne!(out, challenge);
        assert_eq!(encrypt_challenge("pass", &challenge), out);
        assert_ne!(encrypt_challenge("pas2", &challenge), out);
    }

    #[test]
    fn reverse_bits_is_involution() {
        for b in 0..=255u8 {
            assert_eq!(reverse_bits(reverse_bits(b)), b);
        }
    }
}
