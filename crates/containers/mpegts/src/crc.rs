pub(crate) fn mpeg2_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::mpeg2_crc32;

    #[test]
    fn matches_mpeg2_check_value() {
        assert_eq!(mpeg2_crc32(b"123456789"), 0x0376_e6e7);
    }
}
