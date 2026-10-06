const SUFFIXES: &[(&str, f64)] = &[
    ("Ki", 1024.0),
    ("Mi", 1024.0 * 1024.0),
    ("Gi", 1024.0 * 1024.0 * 1024.0),
    ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ("Pi", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ("Ei", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ("k", 1e3),
    ("M", 1e6),
    ("G", 1e9),
    ("T", 1e12),
    ("P", 1e15),
    ("E", 1e18),
];

pub(crate) fn bytes(s: &str) -> u64 {
    let s = s.trim();
    let (number, scale) = SUFFIXES
        .iter()
        .find_map(|&(suffix, scale)| s.strip_suffix(suffix).map(|n| (n, scale)))
        .unwrap_or((s, 1.0));
    match number.trim().parse::<f64>() {
        Ok(n) if n.is_finite() && n >= 0.0 => (n * scale).round() as u64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GI: u64 = 1024 * 1024 * 1024;

    #[test]
    fn binary_suffixes_are_powers_of_two() {
        assert_eq!(bytes("1Ki"), 1024);
        assert_eq!(bytes("1Mi"), 1024 * 1024);
        assert_eq!(bytes("5Gi"), 5 * GI);
        assert_eq!(bytes("2Ti"), 2 * 1024u64.pow(4));
        assert_eq!(bytes("1Pi"), 1024u64.pow(5));
    }

    #[test]
    fn decimal_suffixes_are_powers_of_ten() {
        assert_eq!(bytes("5k"), 5_000);
        assert_eq!(bytes("5M"), 5_000_000);
        assert_eq!(bytes("20G"), 20_000_000_000);
        assert_eq!(bytes("1T"), 1_000_000_000_000);
    }

    #[test]
    fn a_fraction_is_scaled_before_it_is_rounded() {
        assert_eq!(bytes("1.5Gi"), 3 * GI / 2);
        assert_eq!(bytes("0.5Mi"), 512 * 1024);
        assert_eq!(bytes("2.5G"), 2_500_000_000);
    }

    #[test]
    fn a_bare_number_is_a_byte_count() {
        assert_eq!(bytes("1024"), 1024);
        assert_eq!(bytes("0"), 0);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(bytes("  5Gi "), 5 * GI);
        assert_eq!(bytes("5 Gi"), 5 * GI);
    }

    #[test]
    fn anything_unreadable_is_zero_not_a_guess() {
        for junk in ["", "lots", "Gi", "-5Gi", "NaN", "inf", "5Xi"] {
            assert_eq!(bytes(junk), 0, "{junk:?}");
        }
    }
}
