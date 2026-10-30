//! The clock's text. Formatting is Windows' own, so by default it follows the user's
//! regional settings, and custom formats use Windows' patterns ("HH:mm", "ddd d MMM").

use crate::config::{Clock, DateStyle};
use std::time::Duration;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{
    DATE_LONGDATE, DATE_SHORTDATE, ENUM_DATE_FORMATS_FLAGS, GetDateFormatEx, GetTimeFormatEx, TIME_FORMAT_FLAGS, TIME_NOSECONDS,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::{HSTRING, PCWSTR};

pub struct Face {
    /// What the dock shows: the time, and the date on a second line if wanted.
    pub time: String,
    pub date: Option<String>,
    /// Shown on hover: the long date.
    pub tooltip: String,
    /// How long until the text next changes.
    pub next_change: Duration,
}

pub fn now(cfg: &Clock) -> Face {
    face(cfg, &unsafe { GetLocalTime() })
}

pub fn face(cfg: &Clock, t: &SYSTEMTIME) -> Face {
    let pattern = time_pattern(cfg);
    let seconds = pattern.as_deref().map_or(cfg.seconds, |p| p.contains('s'));
    let flags = if seconds { TIME_FORMAT_FLAGS(0) } else { TIME_NOSECONDS };
    // A pattern Windows can't use falls back to the regional format rather than a blank clock.
    let time = pattern.as_deref().and_then(|p| format_time(t, flags, Some(p))).or_else(|| format_time(t, flags, None)).unwrap_or_default();
    let date = match &cfg.date {
        DateStyle::None => None,
        DateStyle::Short => format_date(t, DATE_SHORTDATE, None),
        DateStyle::Long => format_date(t, DATE_LONGDATE, None),
        DateStyle::Custom(p) => format_date(t, ENUM_DATE_FORMATS_FLAGS(0), Some(p)).or_else(|| format_date(t, DATE_SHORTDATE, None)),
    };
    let tooltip = format_date(t, DATE_LONGDATE, None).unwrap_or_default();
    let ms_into_second = t.wMilliseconds as u64;
    let next_change = if seconds {
        Duration::from_millis(1000 - ms_into_second)
    } else {
        Duration::from_millis((60 - t.wSecond as u64) * 1000 - ms_into_second)
    };
    Face { time, date, tooltip, next_change }
}

/// The custom time pattern, with seconds added after the minutes if asked for.
fn time_pattern(cfg: &Clock) -> Option<String> {
    let p = cfg.time.trim();
    if p.is_empty() {
        return None;
    }
    if !cfg.seconds || p.contains('s') {
        return Some(p.to_string());
    }
    Some(match p.rfind('m') {
        Some(i) => format!("{}:ss{}", &p[..=i], &p[i + 1..]),
        None => p.to_string(),
    })
}

fn to_string(buf: &[u16], n: i32) -> Option<String> {
    // n includes the terminating null; 0 means the pattern or the call was rejected.
    (n > 1).then(|| String::from_utf16_lossy(&buf[..n as usize - 1]))
}

fn format_time(t: &SYSTEMTIME, flags: TIME_FORMAT_FLAGS, pattern: Option<&str>) -> Option<String> {
    let pattern = pattern.map(HSTRING::from);
    let mut buf = [0u16; 128];
    let n = unsafe {
        GetTimeFormatEx(PCWSTR::null(), flags, Some(t), pattern.as_ref().map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())), Some(&mut buf))
    };
    to_string(&buf, n)
}

fn format_date(t: &SYSTEMTIME, flags: ENUM_DATE_FORMATS_FLAGS, pattern: Option<&str>) -> Option<String> {
    let pattern = pattern.map(HSTRING::from);
    let mut buf = [0u16; 128];
    let n = unsafe {
        GetDateFormatEx(
            PCWSTR::null(),
            flags,
            Some(t),
            pattern.as_ref().map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            Some(&mut buf),
            PCWSTR::null(),
        )
    };
    to_string(&buf, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u16, m: u16, s: u16, ms: u16) -> SYSTEMTIME {
        SYSTEMTIME { wYear: 2026, wMonth: 10, wDayOfWeek: 5, wDay: 30, wHour: h, wMinute: m, wSecond: s, wMilliseconds: ms }
    }

    fn clock(time: &str, seconds: bool, date: DateStyle) -> Clock {
        Clock { time: time.into(), seconds, date, ..Default::default() }
    }

    #[test]
    fn custom_patterns() {
        let f = face(&clock("HH:mm", false, DateStyle::Custom("yyyy-MM-dd".into())), &at(9, 5, 30, 0));
        assert_eq!(f.time, "09:05");
        assert_eq!(f.date.as_deref(), Some("2026-10-30"));
        assert_eq!(face(&clock("h:mm tt", false, DateStyle::None), &at(21, 5, 0, 0)).time.to_uppercase(), "9:05 PM");
    }

    #[test]
    fn seconds_are_added_after_the_minutes() {
        assert_eq!(face(&clock("HH:mm", true, DateStyle::None), &at(9, 5, 7, 0)).time, "09:05:07");
        assert_eq!(face(&clock("h:mm tt", true, DateStyle::None), &at(9, 5, 7, 0)).time.to_uppercase(), "9:05:07 AM");
    }

    #[test]
    fn wakes_at_the_next_minute_or_second() {
        assert_eq!(face(&clock("HH:mm", false, DateStyle::None), &at(9, 5, 58, 250)).next_change, Duration::from_millis(1750));
        assert_eq!(face(&clock("HH:mm", true, DateStyle::None), &at(9, 5, 58, 250)).next_change, Duration::from_millis(750));
    }

    #[test]
    fn regional_formats() {
        let f = face(&clock("", false, DateStyle::Short), &at(9, 5, 0, 0));
        assert!(f.time.contains("05"), "{}", f.time);
        assert!(f.date.is_some_and(|d| d.contains("30")));
        assert!(f.tooltip.contains("2026"), "{}", f.tooltip);
    }
}
