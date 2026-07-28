use anyhow::{bail, Result};

pub const CRC_WORD_LEN: usize = 3;

pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0xff;
    for byte in data {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x31
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn command_bytes(command: u16) -> [u8; 2] {
    command.to_be_bytes()
}

pub fn command_with_word(command: u16, value: u16) -> [u8; 5] {
    let command = command.to_be_bytes();
    let value = value.to_be_bytes();
    [command[0], command[1], value[0], value[1], crc8(&value)]
}

pub fn decode_words<const N: usize>(wire: &[u8]) -> Result<[[u8; 2]; N]> {
    if wire.len() != N * CRC_WORD_LEN {
        bail!(
            "invalid Sensirion response length: expected {}, got {}",
            N * CRC_WORD_LEN,
            wire.len()
        );
    }

    let mut words = [[0_u8; 2]; N];
    for (index, chunk) in wire.chunks_exact(CRC_WORD_LEN).enumerate() {
        if crc8(&chunk[..2]) != chunk[2] {
            bail!("CRC mismatch in response word {}", index);
        }
        words[index].copy_from_slice(&chunk[..2]);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_known_values() {
        assert_eq!(crc8(&[0x00, 0x00]), 0x81);
        assert_eq!(crc8(&[0x00, 0x02]), 0xe3);
    }

    #[test]
    fn word_encoding_adds_crc() {
        assert_eq!(
            command_with_word(0x6720, 1013),
            [0x67, 0x20, 0x03, 0xf5, 0xdb]
        );
    }

    #[test]
    fn decode_rejects_bad_crc() {
        let error = decode_words::<1>(&[0x00, 0x01, 0x00]).unwrap_err();
        assert!(error.to_string().contains("CRC mismatch"));
    }
}
