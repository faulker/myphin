use crate::error::Error;

/// Parse a SimpleFIN numeric string into integer cents.
///
/// Accepts optional sign, optional fractional part up to 2 digits (pads),
/// and rejects anything that is not a finite decimal amount.
pub fn parse_cents(raw: &str) -> Result<i64, Error> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(Error::user("empty amount"));
    }
    let (neg, rest) = if let Some(r) = s.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = s.strip_prefix('+') {
        (false, r)
    } else {
        (false, s)
    };
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(Error::user(format!("invalid amount: {raw}")));
    }
    let mut parts = rest.split('.');
    let whole = parts.next().unwrap_or("");
    let frac = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err(Error::user(format!("invalid amount: {raw}")));
    }
    if whole.is_empty() && frac.is_empty() {
        return Err(Error::user(format!("invalid amount: {raw}")));
    }
    if frac.len() > 2 {
        return Err(Error::user(format!(
            "amount has more than 2 decimal places: {raw}"
        )));
    }
    let whole_n: i64 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| Error::user(format!("invalid amount: {raw}")))?
    };
    let frac_n: i64 = match frac.len() {
        0 => 0,
        1 => {
            frac.parse::<i64>()
                .map_err(|_| Error::user(format!("invalid amount: {raw}")))?
                * 10
        }
        2 => frac
            .parse()
            .map_err(|_| Error::user(format!("invalid amount: {raw}")))?,
        _ => unreachable!(),
    };
    let cents = whole_n
        .checked_mul(100)
        .and_then(|w| w.checked_add(frac_n))
        .ok_or_else(|| Error::user("amount overflow"))?;
    Ok(if neg { -cents } else { cents })
}

/// Format cents as a signed decimal with two places (no currency symbol).
pub fn format_cents(cents: i64) -> String {
    let neg = cents < 0;
    let abs = cents.unsigned_abs();
    let whole = abs / 100;
    let frac = abs % 100;
    if neg {
        format!("-{whole}.{frac:02}")
    } else {
        format!("{whole}.{frac:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simplefin_samples() {
        assert_eq!(parse_cents("100.23").unwrap(), 10023);
        assert_eq!(parse_cents("-33293.43").unwrap(), -3_329_343);
        assert_eq!(parse_cents("75.23").unwrap(), 7523);
        assert_eq!(parse_cents("0").unwrap(), 0);
        assert_eq!(parse_cents("-0.01").unwrap(), -1);
        assert_eq!(parse_cents("5.2").unwrap(), 520);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_cents("").is_err());
        assert!(parse_cents("1.234").is_err());
        assert!(parse_cents("1.2.3").is_err());
        assert!(parse_cents("abc").is_err());
    }

    #[test]
    fn format_roundtrip() {
        assert_eq!(format_cents(-3329343), "-33293.43");
        assert_eq!(format_cents(10023), "100.23");
        assert_eq!(format_cents(-1), "-0.01");
    }
}
