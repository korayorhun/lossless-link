//! Uncompressed ("escape") ALAC frames: the receiver gets plain 16-bit PCM in an ALAC wrapper, so the stream
//! is bit-exact without an ALAC encoder.

struct BitWriter {
    bytes: Vec<u8>,
    acc: u64,
    bits: u32,
}

impl BitWriter {
    fn with_capacity(n: usize) -> Self {
        Self { bytes: Vec::with_capacity(n), acc: 0, bits: 0 }
    }

    fn put(&mut self, value: u32, width: u32) {
        self.acc = (self.acc << width) | (u64::from(value) & ((1u64 << width) - 1));
        self.bits += width;
        while self.bits >= 8 {
            self.bits -= 8;
            self.bytes.push((self.acc >> self.bits) as u8);
        }
        self.acc &= (1u64 << self.bits) - 1;
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.bytes.push((self.acc << (8 - self.bits)) as u8);
        }
        self.bytes
    }
}

/// Wraps interleaved stereo 16-bit samples in one uncompressed ALAC frame: channel-pair element, has-size
/// and escape flags, the 32-bit frame count, the samples, then the end tag.
pub fn alac_frame(samples: &[i16]) -> Vec<u8> {
    let mut w = BitWriter::with_capacity(samples.len() * 2 + 8);
    w.put(1, 3); // channel pair element
    w.put(0, 4); // element instance tag
    w.put(0, 12); // unused
    w.put(1, 1); // frame size follows
    w.put(0, 2); // no shifted bytes
    w.put(1, 1); // escape: uncompressed samples
    w.put((samples.len() / 2) as u32, 32);
    for &s in samples {
        w.put(u32::from(s as u16), 16);
    }
    w.put(7, 3); // end of frame
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_the_python_reference() {
        let frame = alac_frame(&[0; 704]);
        assert_eq!(&frame[..6], &[0x20, 0x00, 0x12, 0x00, 0x00, 0x02]);
        assert_eq!(frame.len(), 1416); // 55 + 11264 + 3 bits, rounded up to bytes
    }

    #[test]
    fn samples_survive_bit_exact() {
        let samples: Vec<i16> = (0..704).map(|i| (i * 97 - 30000) as i16).collect();
        let frame = alac_frame(&samples);
        let bit = |n: usize| (frame[n / 8] >> (7 - n % 8)) & 1;
        for (i, &s) in samples.iter().enumerate() {
            let mut v = 0u16;
            for b in 0..16 {
                v = (v << 1) | u16::from(bit(55 + i * 16 + b));
            }
            assert_eq!(v as i16, s);
        }
    }
}
