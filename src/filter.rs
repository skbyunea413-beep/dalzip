//! E8/E9 branch filter for x86/x64 machine code.
//!
//! CALL/JMP rel32 operands become absolute targets, so repeated calls to one
//! function turn into repeated byte strings the LZ stage can match. Like BCJ,
//! only operands that look like near branches (|rel| < 16 MiB, i.e. the top byte
//! is 0x00 or 0xFF) are touched, which keeps data that merely contains 0xE8
//! intact. The conversion is addition modulo 2^25 inside that same range, so a
//! value is in range before encoding exactly when it is in range after, and the
//! decoder makes the same decisions as the encoder.
//!
//! Encoder and decoder visit the same opcode positions because only the four
//! operand bytes after an opcode are rewritten, and both skip over them.

const HALF: i64 = 1 << 24;

fn wrap(v: i64) -> i64 {
    (v + HALF).rem_euclid(2 * HALF) - HALF
}

pub fn e8e9_encode(src: &[u8]) -> Vec<u8> {
    let mut b = src.to_vec();
    transform(&mut b, true);
    b
}

pub fn e8e9_decode(buf: &mut [u8]) {
    transform(buf, false);
}

fn transform(b: &mut [u8], encode: bool) {
    if b.len() < 5 {
        return;
    }
    let end = b.len() - 4;
    let mut i = 0;
    while i < end {
        if b[i] != 0xE8 && b[i] != 0xE9 {
            i += 1;
            continue;
        }
        let top = b[i + 4];
        if top == 0x00 || top == 0xFF {
            let x = i32::from_le_bytes(b[i + 1..i + 5].try_into().unwrap()) as i64;
            let pos = i as i64 + 5;
            let y = if encode { wrap(x + pos) } else { wrap(x - pos) };
            b[i + 1..i + 5].copy_from_slice(&(y as i32).to_le_bytes());
        }
        i += 5;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_random_and_edge_operands() {
        let mut data = Vec::new();
        let mut seed = 0x1234_5678_9abc_def0u64;
        for _ in 0..200_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            data.push(match seed % 11 {
                0 => 0xE8,
                1 => 0x00,
                2 => 0xFF,
                _ => seed as u8,
            });
        }
        for (k, v) in [i32::MIN, i32::MAX, 0, -1, (HALF - 1) as i32, HALF as i32, -(HALF as i32), -(HALF as i32) - 1]
            .into_iter()
            .enumerate()
        {
            let at = 1000 + k * 10;
            data[at] = 0xE9;
            data[at + 1..at + 5].copy_from_slice(&v.to_le_bytes());
        }
        let mut enc = e8e9_encode(&data);
        assert_ne!(enc, data);
        e8e9_decode(&mut enc);
        assert_eq!(enc, data);
    }
}
