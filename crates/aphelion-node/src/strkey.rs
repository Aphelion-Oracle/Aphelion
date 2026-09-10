//! Stellar strkey decoding.
//!
//! Only what the node needs: turning a `C...` contract address into the raw
//! 32 bytes that go into the signing payload.
//!
//! This lives in the library rather than in the binary because the signature a
//! node produces is only accepted by a party that derives the *same* 32 bytes
//! from the *same* address. A second implementation that disagreed would not
//! fail loudly -- it would produce signatures that verify nowhere, which is the
//! most expensive class of bug this repository has. One decoder, shared.

use crate::error::{NodeError, Result};

/// Derive the raw 32-byte contract id that goes into the signing payload.
///
/// A `C...` address is a 56-character strkey: a version byte, 32 payload
/// bytes and a 2-byte CRC, base32-encoded. Decoding it here rather than
/// pulling in a strkey dependency keeps the node's dependency surface small,
/// and the checksum is verified so a typo in a config file fails loudly
/// instead of producing signatures that nothing will ever accept.
pub fn contract_id_bytes(address: &str) -> Result<[u8; 32]> {
    const VERSION_BYTE_CONTRACT: u8 = 2 << 3; // 'C'

    let decoded = base32_decode(address)
        .ok_or_else(|| NodeError::Config(format!("`{address}` is not valid base32")))?;

    if decoded.len() != 35 {
        return Err(NodeError::Config(format!(
            "`{address}` decodes to {} bytes, expected 35",
            decoded.len()
        )));
    }
    if decoded[0] != VERSION_BYTE_CONTRACT {
        return Err(NodeError::Config(format!(
            "`{address}` is not a contract address (wrong version byte)"
        )));
    }

    let expected = u16::from_le_bytes([decoded[33], decoded[34]]);
    let actual = crc16_xmodem(&decoded[..33]);
    if expected != actual {
        return Err(NodeError::Config(format!(
            "`{address}` has a bad checksum; check for a transcription error"
        )));
    }

    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded[1..33]);
    Ok(out)
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 5 / 8);

    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let value = ALPHABET.iter().position(|&a| a == c)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_real_contract_address() {
        // Native XLM SAC on testnet.
        let addr = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
        let id = contract_id_bytes(addr).expect("valid address");
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn rejects_an_account_address() {
        let account = "GCEZWKCA5VLDNRLN3RPRJMRZOX3Z6G5CHCGSNFHEYVXM3XOJMDS674JZ";
        let err = contract_id_bytes(account).unwrap_err().to_string();
        assert!(err.contains("not a contract address"), "{err}");
    }

    #[test]
    fn rejects_a_transcription_error() {
        // Flip one character of a valid address; the CRC must catch it.
        let addr = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSD";
        let err = contract_id_bytes(addr).unwrap_err().to_string();
        assert!(err.contains("checksum"), "{err}");
    }

    #[test]
    fn crc16_matches_the_known_xmodem_vector() {
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
    }
}
