//! Decrypt the first bytes of an encrypted ZIP entry (WinZip AES or ZipCrypto), so the
//! compression header inside (LZMA/XZ dictionary, PPMd model size) can be checked against
//! the decoder memory limit before the entry is decoded.

use aes::cipher::{BlockCipherEncrypt, BlockSizeUser, KeyInit};
use zeroize::Zeroizing;
use zip::AesMode;

/// The first `n` bytes of an AES entry's data. `raw` is the entry's stored data: salt, the
/// 2-byte password verifier, then the encrypted data. `None` when the verifier does not
/// match `password` or `raw` is too short.
pub(crate) fn aes_head(raw: &[u8], mode: AesMode, password: &[u8], n: usize) -> Option<Vec<u8>> {
    let key_len = match mode {
        AesMode::Aes128 => 16,
        AesMode::Aes192 => 24,
        AesMode::Aes256 => 32,
    };
    let salt_len = key_len / 2;
    let salt = raw.get(..salt_len)?;
    let verifier = raw.get(salt_len..salt_len + 2)?;
    // Encryption key, authentication key, verifier (WinZip AE-1/AE-2).
    let mut derived = Zeroizing::new(vec![0u8; key_len * 2 + 2]);
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, 1000, &mut derived);
    if derived[key_len * 2..] != *verifier {
        return None;
    }
    let data = &raw[salt_len + 2..];
    let mut out = data[..data.len().min(n)].to_vec();
    let key = &derived[..key_len];
    match key_len {
        16 => ctr::<aes::Aes128>(key, &mut out),
        24 => ctr::<aes::Aes192>(key, &mut out),
        _ => ctr::<aes::Aes256>(key, &mut out),
    }
    Some(out)
}

/// WinZip's AES-CTR: a little-endian block counter starting at 1, no nonce.
fn ctr<C: KeyInit + BlockCipherEncrypt + BlockSizeUser>(key: &[u8], data: &mut [u8]) {
    let Ok(cipher) = C::new_from_slice(key) else {
        return;
    };
    for (i, chunk) in data.chunks_mut(16).enumerate() {
        let mut block = aes::cipher::Block::<C>::default();
        block.copy_from_slice(&(i as u128 + 1).to_le_bytes());
        cipher.encrypt_block(&mut block);
        for (b, k) in chunk.iter_mut().zip(block.iter()) {
            *b ^= k;
        }
    }
}

/// The first `n` bytes of a ZipCrypto entry's data. `raw` starts with the 12-byte
/// encryption header.
pub(crate) fn zipcrypto_head(raw: &[u8], password: &[u8], n: usize) -> Vec<u8> {
    let mut keys = ZipCryptoKeys::new(password);
    raw.iter()
        .take(12 + n)
        .map(|&c| keys.decrypt(c))
        .skip(12)
        .collect()
}

struct ZipCryptoKeys([u32; 3]);

impl ZipCryptoKeys {
    fn new(password: &[u8]) -> Self {
        let mut keys = Self([0x1234_5678, 0x2345_6789, 0x3456_7890]);
        for &b in password {
            keys.update(b);
        }
        keys
    }

    fn update(&mut self, b: u8) {
        let k = &mut self.0;
        k[0] = crc32_byte(k[0], b);
        k[1] = k[1]
            .wrapping_add(k[0] & 0xFF)
            .wrapping_mul(134_775_813)
            .wrapping_add(1);
        k[2] = crc32_byte(k[2], (k[1] >> 24) as u8);
    }

    fn decrypt(&mut self, c: u8) -> u8 {
        let t = (self.0[2] | 2) as u16;
        let plain = c ^ (t.wrapping_mul(t ^ 1) >> 8) as u8;
        self.update(plain);
        plain
    }
}

fn crc32_byte(crc: u32, b: u8) -> u32 {
    let mut c = (crc ^ u32::from(b)) & 0xFF;
    for _ in 0..8 {
        c = if c & 1 != 0 {
            0xEDB8_8320 ^ (c >> 1)
        } else {
            c >> 1
        };
    }
    (crc >> 8) ^ c
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use zip::unstable::write::FileOptionsExt;
    use zip::write::SimpleFileOptions;

    /// The stored (raw) data of entry 0 of a zip in memory.
    fn raw_of(bytes: &[u8]) -> Vec<u8> {
        let mut za = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut raw = Vec::new();
        za.by_index_raw(0).unwrap().read_to_end(&mut raw).unwrap();
        raw
    }

    #[test]
    fn heads_decrypt_like_the_zip_crate_writes_them() {
        let plain = b"PLAINTEXT header bytes, then more and more data......";
        for legacy in [false, true] {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let o = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            let o = if legacy {
                o.with_deprecated_encryption(b"pw").unwrap()
            } else {
                o.with_aes_encryption(AesMode::Aes256, "pw")
            };
            zw.start_file("x", o).unwrap();
            zw.write_all(plain).unwrap();
            let bytes = zw.finish().unwrap().into_inner();
            let raw = raw_of(&bytes);
            let head = if legacy {
                zipcrypto_head(&raw, b"pw", 16)
            } else {
                aes_head(&raw, AesMode::Aes256, b"pw", 16).unwrap()
            };
            assert_eq!(head, plain[..16], "legacy {legacy}");
            if !legacy {
                assert!(aes_head(&raw, AesMode::Aes256, b"wrong", 16).is_none());
            }
        }
    }
}
