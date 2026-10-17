//! Text formatting that matches Finder and follows the user's locale.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2_foundation::{
    NSByteCountFormatter, NSByteCountFormatterCountStyle, NSDate, NSDateFormatter,
    NSDateFormatterStyle, NSNumber, NSNumberFormatter, NSNumberFormatterStyle,
    NSRelativeDateTimeFormatter,
};

/// Sizes exactly as Finder shows them: decimal units in the user's number format.
pub fn bytes(n: u64) -> String {
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    NSByteCountFormatter::stringFromByteCount_countStyle(n, NSByteCountFormatterCountStyle::File)
        .to_string()
}

thread_local! {
    static COUNT: Retained<NSNumberFormatter> = {
        let f = NSNumberFormatter::new();
        f.setNumberStyle(NSNumberFormatterStyle::DecimalStyle);
        f
    };
}

/// Item counts with the user's grouping separator.
pub fn count(n: u64) -> String {
    COUNT.with(|f| f.stringFromNumber(&NSNumber::new_u64(n)))
        .map_or_else(|| n.to_string(), |s| s.to_string())
}

fn ns_date(t: SystemTime) -> Retained<NSDate> {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    NSDate::dateWithTimeIntervalSince1970(secs)
}

/// Modification date in the user's locale, for example "17 Oct 2026 at 11:42".
pub fn date(unix_secs: u32) -> String {
    if unix_secs == 0 {
        return "—".into();
    }
    let f = NSDateFormatter::new();
    f.setDateStyle(NSDateFormatterStyle::MediumStyle);
    f.setTimeStyle(NSDateFormatterStyle::ShortStyle);
    let t = UNIX_EPOCH + Duration::from_secs(u64::from(unix_secs));
    f.stringFromDate(&ns_date(t)).to_string()
}

/// "2 hours ago", in the user's locale.
pub fn ago(t: SystemTime) -> String {
    if SystemTime::now().duration_since(t).unwrap_or_default() < Duration::from_secs(60) {
        return "just now".into();
    }
    let f = NSRelativeDateTimeFormatter::new();
    f.localizedStringForDate_relativeToDate(&ns_date(t), &NSDate::now())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digits(s: &str) -> String {
        s.chars().filter(char::is_ascii_digit).collect()
    }

    #[test]
    fn sizes_use_decimal_units_like_finder() {
        // Finder counts 1 MB as 1,000,000 bytes. Binary units would show 977 KB here and
        // disagree with the free space Finder reports.
        assert!(bytes(1_000_000).starts_with('1'), "{}", bytes(1_000_000));
        assert!(bytes(1_000_000_000).starts_with('1'), "{}", bytes(1_000_000_000));
        // Large values must not overflow the formatter's signed input.
        assert!(!bytes(u64::MAX).is_empty());
    }

    #[test]
    fn counts_keep_every_digit_and_group_thousands() {
        assert_eq!(count(999), "999");
        let big = count(1_234_567);
        assert_eq!(digits(&big), "1234567");
        assert!(big.len() > 7, "no grouping separator in {big}");
    }
}
