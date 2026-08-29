//! Byte-aligned start-code utilities.

/// Finds the next MPEG-style `00 00 01` start-code prefix.
#[must_use]
pub fn find_start_code_prefix(data: &[u8], from: usize) -> Option<usize> {
    if from >= data.len() {
        return None;
    }

    data[from..]
        .windows(3)
        .position(|window| window == [0, 0, 1])
        .map(|position| from + position)
}

#[cfg(test)]
mod tests {
    use super::find_start_code_prefix;

    #[test]
    fn locates_prefix() {
        assert_eq!(find_start_code_prefix(&[9, 0, 0, 1, 0xb3], 0), Some(1));
        assert_eq!(find_start_code_prefix(&[9, 0, 0, 1, 0xb3], 2), None);
    }
}
