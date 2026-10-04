//! Bounded Gregorian UTC dates; dates do not imply a time of day or timezone.
use crate::Result;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn day(text: &str) -> Result<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return Err("Use a date in YYYY-MM-DD format".into());
    }
    let year: i64 = text[..4].parse().map_err(|_| "Invalid date year")?;
    let month: usize = text[5..7].parse().map_err(|_| "Invalid date month")?;
    let date: i64 = text[8..].parse().map_err(|_| "Invalid date day")?;
    if !(1800..=9999).contains(&year) || !(1..=12).contains(&month) {
        return Err("Date is outside the supported range".into());
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let lengths = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=lengths[month - 1]).contains(&date) {
        return Err("Invalid calendar day".into());
    }
    let prior = year - 1;
    Ok(prior * 365 + prior / 4 - prior / 100
        + prior / 400
        + lengths[..month - 1].iter().sum::<i64>()
        + date
        - 1
        - 719_162)
}

pub fn from_day(days: i64) -> Result<String> {
    if !(day("1800-01-01")?..=day("9999-12-31")?).contains(&days) {
        return Err("Date is outside the supported range".into());
    }
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    Ok(format!("{year:04}-{month:02}-{date:02}"))
}

pub fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    i64::try_from(days)
        .ok()
        .and_then(|days| from_day(days).ok())
        .unwrap_or_else(|| "9999-12-31".into())
}

pub fn add(text: &str, days: i64) -> Result<String> {
    from_day(day(text)?.checked_add(days).ok_or("Date overflow")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dates_round_trip_across_leap_centuries_and_epoch() {
        for text in [
            "1800-01-01",
            "1900-02-28",
            "1969-12-31",
            "1970-01-01",
            "2000-02-29",
            "2026-10-04",
            "9999-12-31",
        ] {
            assert_eq!(from_day(day(text).unwrap()).unwrap(), text);
        }
        assert_eq!(day("1970-01-01").unwrap(), 0);
        assert_eq!(add("2024-02-28", 1).unwrap(), "2024-02-29");
        assert_eq!(add("1900-02-28", 1).unwrap(), "1900-03-01");
        for text in [
            "2023-02-29",
            "1900-02-29",
            "2026-13-01",
            "2026-10-00",
            "1799-12-31",
            "2026-1-01",
            "é026-10-01",
        ] {
            assert!(day(text).is_err());
        }
        assert!(add("9999-12-31", 1).is_err());
    }
}
