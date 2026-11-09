//! Sizes (of memory and disks) as people write and read them: `2G`, `512M`, `1.5 GiB`.

/// Parses a size: a number of bytes, optionally with a binary suffix (K, M, G or T, also
/// as KiB, ..., in any case).
pub fn parse_size(text: &str) -> Result<u64, String> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(digits);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid size {text:?}"))?;
    let shift = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kib" => 10,
        "m" | "mib" => 20,
        "g" | "gib" => 30,
        "t" | "tib" => 40,
        _ => return Err(format!("invalid size suffix {suffix:?} (use K, M, G or T)")),
    };
    number
        .checked_mul(1 << shift)
        .ok_or_else(|| format!("size {text:?} is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("512M"), Ok(512 << 20));
        assert_eq!(parse_size("2g"), Ok(2 << 30));
        assert_eq!(parse_size("1TiB"), Ok(1 << 40));
        for invalid in ["", "G", "1.5G", "2X", "-1", "99999999999T"] {
            assert!(parse_size(invalid).is_err(), "{invalid:?}");
        }
    }
}
