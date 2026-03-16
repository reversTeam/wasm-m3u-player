/// Bitstream reader for AC-3/E-AC-3 frame parsing.
pub struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0-7, bits consumed in current byte (MSB first)
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    /// Read up to 25 bits (returns u32).
    #[inline]
    pub fn read(&mut self, n: u8) -> u32 {
        debug_assert!(n <= 25);
        let mut val: u32 = 0;
        let mut bits_left = n;

        while bits_left > 0 {
            if self.byte_pos >= self.data.len() {
                return val << bits_left; // pad with zeros at end
            }

            let available = 8 - self.bit_pos;
            let take = bits_left.min(available);

            let byte = self.data[self.byte_pos] as u32;
            let shift = available - take;
            let mask = ((1u32 << take) - 1) << shift;
            let bits = (byte & mask) >> shift;

            val = (val << take) | bits;
            bits_left -= take;
            self.bit_pos += take;

            if self.bit_pos >= 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
        }

        val
    }

    /// Read n bits as a signed two's complement integer.
    /// Used for mantissa values (BAP 6-15).
    #[inline]
    pub fn read_signed(&mut self, n: u8) -> i32 {
        let val = self.read(n) as i32;
        let sign_bit = 1i32 << (n - 1);
        if val >= sign_bit {
            val - (sign_bit << 1)
        } else {
            val
        }
    }

    /// Read a single bit as bool.
    #[inline]
    pub fn read_bool(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Skip n bits.
    #[inline]
    pub fn skip(&mut self, n: usize) {
        let total_bits = self.bit_pos as usize + n;
        self.byte_pos += total_bits / 8;
        self.bit_pos = (total_bits % 8) as u8;
    }

    /// Current bit position from start of data.
    #[inline]
    pub fn position(&self) -> usize {
        self.byte_pos * 8 + self.bit_pos as usize
    }

    /// Remaining bits.
    #[inline]
    pub fn remaining(&self) -> usize {
        if self.byte_pos >= self.data.len() {
            return 0;
        }
        self.data.len() * 8 - self.position()
    }

    /// Align to next byte boundary.
    #[inline]
    pub fn align(&mut self) {
        if self.bit_pos != 0 {
            self.byte_pos += 1;
            self.bit_pos = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_basic() {
        let data = [0b10110011, 0b01010101];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read(4), 0b1011);
        assert_eq!(br.read(4), 0b0011);
        assert_eq!(br.read(8), 0b01010101);
    }

    #[test]
    fn read_cross_byte() {
        let data = [0b11001100, 0b10101010];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read(3), 0b110);
        assert_eq!(br.read(6), 0b011001);
        assert_eq!(br.read(7), 0b0101010);
    }

    #[test]
    fn read_signed_positive() {
        // 5 bits: 01010 = 10 (positive)
        let data = [0b01010_000];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_signed(5), 10);
    }

    #[test]
    fn read_signed_negative() {
        // 5 bits: 11110 = -2 in two's complement
        let data = [0b11110_000];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_signed(5), -2);
    }

    #[test]
    fn read_signed_minus_one() {
        // 5 bits: 11111 = -1
        let data = [0b11111_000];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_signed(5), -1);
    }

    #[test]
    fn read_bool_and_skip() {
        let data = [0b10000000];
        let mut br = BitReader::new(&data);
        assert!(br.read_bool());
        assert!(!br.read_bool());
        br.skip(6);
        assert_eq!(br.position(), 8);
    }
}
