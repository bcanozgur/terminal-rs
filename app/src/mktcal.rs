//! Market calendar: trading sessions, holidays and early closes of the NYSE and Borsa
//! Istanbul, computed locally (no data source, nothing to buy or fetch).
//!
//! * NYSE holidays follow the exchange's rules: fixed-date holidays that fall on a Sunday move
//!   to Monday and those on a Saturday to Friday, except New Year's Day (a Saturday 1 January
//!   is *not* observed on the Friday before). Good Friday is computed from Easter. Early
//!   closes (13:00 ET): the day after Thanksgiving, 3 July when it is a weekday and 4 July is
//!   too, and 24 December on Monday to Thursday. One-off closures (days of mourning, storms)
//!   cannot be known in advance.
//! * Borsa Istanbul closes on the fixed public holidays (1 Jan, 23 Apr, 1 May, 19 May, 15 Jul,
//!   30 Aug, 29 Oct); holidays that fall on a weekend are not made up. The half-day (market
//!   closes 13:00) is the afternoon before each holiday that has an eve: the two bayrams and
//!   28 Oct.
//! * The religious holidays (Ramazan and Kurban Bayramı) move every year, so they are listed
//!   explicitly. Sources: Borsa Istanbul's published 2026 holiday tables (Pay Piyasası 2026
//!   Yılı Tatil Tablosu, borsaistanbul.com) for 2026, and the Diyanet religious calendar as
//!   reported by Turkish press (CNN Türk, Dünya, Milliyet, Aug 2026) for 2027. Years outside
//!   the list report `religious_known == false`; add them when Borsa Istanbul publishes the
//!   next table. Extra "bridge" days the government adds to a bayram are announced late
//!   and are not included.
//!
//! Time zones: New York follows the US rule (second Sunday of March to first Sunday of
//! November, switching at 02:00 local); Istanbul is UTC+3 all year since 2016.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc, Weekday};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    Nyse,
    Bist,
}

impl Exchange {
    pub fn label(self) -> &'static str {
        match self {
            Exchange::Nyse => "NYSE",
            Exchange::Bist => "Borsa Istanbul",
        }
    }
}

/// Exchange of a broker symbol: Borsa Istanbul tickers carry `.IS` (or `.E`), everything else
/// is treated as US.
pub fn exchange_for_symbol(symbol: &str) -> Exchange {
    let s = symbol.trim().to_ascii_uppercase();
    if s.ends_with(".IS") || s.ends_with(".E") { Exchange::Bist } else { Exchange::Nyse }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Closed,
    /// Pre-market (NYSE) or the opening session (BIST).
    Pre,
    Regular,
    /// After-hours (NYSE).
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayKind {
    Closed,
    /// Trading closes early, at this local time.
    Early(NaiveTime),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holiday {
    pub date: NaiveDate,
    pub name: String,
    pub kind: DayKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarketStatus {
    pub exchange: Exchange,
    pub phase: Phase,
    /// `NYSE is closed (Thanksgiving Day)`, `NYSE is in after-hours trading`, …
    pub note: String,
}

impl MarketStatus {
    pub fn is_regular(&self) -> bool {
        self.phase == Phase::Regular
    }
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).expect("valid calendar date")
}

fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).expect("valid time")
}

/// The `n`th (1-based) `wd` of the month.
fn nth_weekday(y: i32, m: u32, wd: Weekday, n: u32) -> NaiveDate {
    let first = d(y, m, 1);
    let shift = (7 + wd.num_days_from_monday() - first.weekday().num_days_from_monday()) % 7;
    first + Duration::days(i64::from(shift + 7 * (n - 1)))
}

fn last_weekday(y: i32, m: u32, wd: Weekday) -> NaiveDate {
    let next = if m == 12 { d(y + 1, 1, 1) } else { d(y, m + 1, 1) };
    let last = next - Duration::days(1);
    let back = (7 + last.weekday().num_days_from_monday() - wd.num_days_from_monday()) % 7;
    last - Duration::days(i64::from(back))
}

/// Easter Sunday (anonymous Gregorian algorithm).
pub fn easter(y: i32) -> NaiveDate {
    let a = y % 19;
    let b = y / 100;
    let c = y % 100;
    let dd = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - dd - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    d(y, month as u32, day as u32)
}

/// NYSE observance: Saturday → Friday, Sunday → Monday.
fn observed(date: NaiveDate) -> NaiveDate {
    match date.weekday() {
        Weekday::Sat => date - Duration::days(1),
        Weekday::Sun => date + Duration::days(1),
        _ => date,
    }
}

fn is_weekend(date: NaiveDate) -> bool {
    matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
}

fn nyse_holidays(y: i32) -> Vec<Holiday> {
    let closed = |date, name: &str| Holiday { date, name: name.into(), kind: DayKind::Closed };
    let early = |date, name: &str| Holiday { date, name: name.into(), kind: DayKind::Early(t(13, 0)) };
    let mut v = Vec::new();
    // A Saturday 1 January is not observed on the Friday (that day is 31 Dec of last year).
    let ny = d(y, 1, 1);
    if ny.weekday() != Weekday::Sat {
        v.push(closed(observed(ny), "New Year's Day"));
    }
    v.push(closed(nth_weekday(y, 1, Weekday::Mon, 3), "Martin Luther King Jr. Day"));
    v.push(closed(nth_weekday(y, 2, Weekday::Mon, 3), "Presidents' Day"));
    v.push(closed(easter(y) - Duration::days(2), "Good Friday"));
    v.push(closed(last_weekday(y, 5, Weekday::Mon), "Memorial Day"));
    if y >= 2022 {
        v.push(closed(observed(d(y, 6, 19)), "Juneteenth"));
    }
    let jul4 = d(y, 7, 4);
    v.push(closed(observed(jul4), "Independence Day"));
    let jul3 = d(y, 7, 3);
    if !is_weekend(jul3) && !is_weekend(jul4) {
        v.push(early(jul3, "Early close before Independence Day"));
    }
    v.push(closed(nth_weekday(y, 9, Weekday::Mon, 1), "Labor Day"));
    let thanks = nth_weekday(y, 11, Weekday::Thu, 4);
    v.push(closed(thanks, "Thanksgiving Day"));
    v.push(early(thanks + Duration::days(1), "Day after Thanksgiving"));
    let xmas = d(y, 12, 25);
    v.push(closed(observed(xmas), "Christmas Day"));
    let eve = d(y, 12, 24);
    if matches!(eve.weekday(), Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu) {
        v.push(early(eve, "Christmas Eve"));
    }
    // New Year's Day of next year observed on 31 December never applies (see above).
    v.sort_by_key(|h| h.date);
    v
}

/// Religious holidays of Borsa Istanbul as (year, eve, first day, last day). See the module
/// docs for the sources. The eve is a half day when it is a weekday; days that fall on a
/// weekend simply add nothing.
type Bayram = (i32, &'static str, (u32, u32), (u32, u32), (u32, u32));
const BIST_BAYRAMS: &[Bayram] = &[
    (2026, "Ramazan Bayramı", (3, 19), (3, 20), (3, 22)),
    (2026, "Kurban Bayramı", (5, 26), (5, 27), (5, 30)),
    (2027, "Ramazan Bayramı", (3, 8), (3, 9), (3, 11)),
    (2027, "Kurban Bayramı", (5, 15), (5, 16), (5, 19)),
];

/// Religious holidays are listed for this year.
pub fn religious_known(y: i32) -> bool {
    BIST_BAYRAMS.iter().any(|b| b.0 == y)
}

fn bist_holidays(y: i32) -> Vec<Holiday> {
    let closed = |date, name: &str| Holiday { date, name: name.into(), kind: DayKind::Closed };
    let mut v = Vec::new();
    for (m, day, name) in [
        (1, 1, "New Year's Day"),
        (4, 23, "National Sovereignty and Children's Day"),
        (5, 1, "Labour and Solidarity Day"),
        (5, 19, "Commemoration of Atatürk, Youth and Sports Day"),
        (7, 15, "Democracy and National Unity Day"),
        (8, 30, "Victory Day"),
        (10, 29, "Republic Day"),
    ] {
        v.push(closed(d(y, m, day), name));
    }
    v.push(Holiday { date: d(y, 10, 28), name: "Republic Day eve (half day)".into(), kind: DayKind::Early(t(13, 0)) });
    for &(by, name, eve, first, last) in BIST_BAYRAMS {
        if by != y {
            continue;
        }
        v.push(Holiday {
            date: d(y, eve.0, eve.1),
            name: format!("{name} eve (half day)"),
            kind: DayKind::Early(t(13, 0)),
        });
        let (mut day, end) = (d(y, first.0, first.1), d(y, last.0, last.1));
        while day <= end {
            v.push(closed(day, name));
            day += Duration::days(1);
        }
    }
    // A fixed holiday that is also inside a bayram appears once, as the closure.
    v.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| (b.kind == DayKind::Closed).cmp(&(a.kind == DayKind::Closed))));
    v.dedup_by_key(|h| h.date);
    // Drop half days that land on a weekend; closures on weekends stay listed as information
    // only through `upcoming`, which filters them, so remove them here too.
    v.retain(|h| !is_weekend(h.date));
    v
}

/// Holidays and early closes of `year`, in date order, for weekdays only.
pub fn holidays(ex: Exchange, year: i32) -> Vec<Holiday> {
    let mut v = match ex {
        Exchange::Nyse => nyse_holidays(year),
        Exchange::Bist => bist_holidays(year),
    };
    v.retain(|h| !is_weekend(h.date));
    v
}

/// The holiday or early close on `date`, if any.
pub fn day_info(ex: Exchange, date: NaiveDate) -> Option<Holiday> {
    holidays(ex, date.year()).into_iter().find(|h| h.date == date)
}

/// Holidays and early closes from `from` through the end of next year.
pub fn upcoming(ex: Exchange, from: NaiveDate) -> Vec<Holiday> {
    (from.year()..=from.year() + 1).flat_map(|y| holidays(ex, y)).filter(|h| h.date >= from).collect()
}

/// Next regular trading day after `date`.
pub fn next_open_day(ex: Exchange, date: NaiveDate) -> NaiveDate {
    let mut day = date + Duration::days(1);
    loop {
        if !is_weekend(day) && !day_info(ex, day).is_some_and(|h| h.kind == DayKind::Closed) {
            return day;
        }
        day += Duration::days(1);
    }
}

// ---- time zones -------------------------------------------------------------------------

/// Seconds east of UTC in New York at the UTC instant `utc`.
pub fn ny_offset_secs(utc: NaiveDateTime) -> i64 {
    let y = utc.year();
    // DST starts 02:00 EST (07:00 UTC) on the second Sunday of March and ends 02:00 EDT
    // (06:00 UTC) on the first Sunday of November.
    let start = nth_weekday(y, 3, Weekday::Sun, 2).and_hms_opt(7, 0, 0).expect("time");
    let end = nth_weekday(y, 11, Weekday::Sun, 1).and_hms_opt(6, 0, 0).expect("time");
    if utc >= start && utc < end { -4 * 3600 } else { -5 * 3600 }
}

/// Exchange-local wall clock time at `now`.
pub fn local_time(ex: Exchange, now: DateTime<Utc>) -> NaiveDateTime {
    let utc = now.naive_utc();
    match ex {
        Exchange::Nyse => utc + Duration::seconds(ny_offset_secs(utc)),
        Exchange::Bist => utc + Duration::hours(3),
    }
}

/// The market's state at `now`.
pub fn status(ex: Exchange, now: DateTime<Utc>) -> MarketStatus {
    let local = local_time(ex, now);
    let (date, time) = (local.date(), local.time());
    let name = ex.label();
    let result = |phase, note: String| MarketStatus { exchange: ex, phase, note };
    if is_weekend(date) {
        let reopen = next_open_day(ex, date);
        return result(
            Phase::Closed,
            format!("{name} is closed for the weekend (reopens {})", reopen.format("%a %b %-d")),
        );
    }
    let info = day_info(ex, date);
    if let Some(h) = info.as_ref().filter(|h| h.kind == DayKind::Closed) {
        let reopen = next_open_day(ex, date);
        return result(
            Phase::Closed,
            format!("{name} is closed today ({}); reopens {}", h.name, reopen.format("%a %b %-d")),
        );
    }
    let close = match info.as_ref().map(|h| h.kind) {
        Some(DayKind::Early(c)) => c,
        _ => match ex {
            Exchange::Nyse => t(16, 0),
            Exchange::Bist => t(18, 0),
        },
    };
    let (pre_open, open, after_end) = match ex {
        Exchange::Nyse => (t(4, 0), t(9, 30), if close < t(16, 0) { t(17, 0) } else { t(20, 0) }),
        Exchange::Bist => (t(9, 40), t(10, 0), close),
    };
    let early = if info.is_some() { " (early close)" } else { "" };
    if time < pre_open {
        result(Phase::Closed, format!("{name} is closed until {} local time", pre_open.format("%H:%M")))
    } else if time < open {
        let what = if ex == Exchange::Nyse { "pre-market" } else { "its opening session" };
        result(Phase::Pre, format!("{name} is in {what}; regular trading starts at {}", open.format("%H:%M")))
    } else if time < close {
        result(Phase::Regular, format!("{name} is open{early}, closing at {}", close.format("%H:%M")))
    } else if time < after_end {
        result(Phase::After, format!("{name} is in after-hours trading{early}"))
    } else {
        let reopen = next_open_day(ex, date);
        result(Phase::Closed, format!("{name} is closed; reopens {}", reopen.format("%a %b %-d")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, day: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, day, h, min, 0).unwrap()
    }

    fn names(ex: Exchange, y: i32) -> Vec<(NaiveDate, String)> {
        holidays(ex, y).into_iter().map(|h| (h.date, h.name)).collect()
    }

    fn has(ex: Exchange, date: NaiveDate, kind: DayKind) -> bool {
        day_info(ex, date).is_some_and(|h| h.kind == kind)
    }

    #[test]
    fn easter_dates() {
        assert_eq!(easter(2026), d(2026, 4, 5));
        assert_eq!(easter(2027), d(2027, 3, 28));
        assert_eq!(easter(2024), d(2024, 3, 31));
    }

    #[test]
    fn good_friday_2027_and_2026() {
        assert!(has(Exchange::Nyse, d(2027, 3, 26), DayKind::Closed));
        assert!(has(Exchange::Nyse, d(2026, 4, 3), DayKind::Closed));
    }

    #[test]
    fn thanksgiving_and_the_day_after() {
        assert!(has(Exchange::Nyse, d(2026, 11, 26), DayKind::Closed));
        assert!(has(Exchange::Nyse, d(2026, 11, 27), DayKind::Early(t(13, 0))));
        assert!(has(Exchange::Nyse, d(2027, 11, 25), DayKind::Closed));
        assert!(has(Exchange::Nyse, d(2027, 11, 26), DayKind::Early(t(13, 0))));
    }

    #[test]
    fn independence_day_observed() {
        // 2026: 4 July is a Saturday, observed Friday 3 July, and no early close.
        assert!(has(Exchange::Nyse, d(2026, 7, 3), DayKind::Closed));
        assert!(day_info(Exchange::Nyse, d(2026, 7, 2)).is_none());
        // 2027: 4 July is a Sunday, observed Monday 5 July; Friday 2 July is a normal day.
        assert!(has(Exchange::Nyse, d(2027, 7, 5), DayKind::Closed));
        // 2025: Thursday 3 July closes early, Friday 4 July is closed.
        assert!(has(Exchange::Nyse, d(2025, 7, 3), DayKind::Early(t(13, 0))));
        assert!(has(Exchange::Nyse, d(2025, 7, 4), DayKind::Closed));
    }

    #[test]
    fn christmas_and_new_year_edges() {
        // 2027: 25 Dec is a Saturday → closed Friday 24 Dec (no early close that day).
        assert!(has(Exchange::Nyse, d(2027, 12, 24), DayKind::Closed));
        // 2026: Christmas Eve is a Thursday → early close; Christmas Friday closed.
        assert!(has(Exchange::Nyse, d(2026, 12, 24), DayKind::Early(t(13, 0))));
        assert!(has(Exchange::Nyse, d(2026, 12, 25), DayKind::Closed));
        // 1 Jan 2028 is a Saturday: not observed on Friday 31 Dec 2027.
        assert!(day_info(Exchange::Nyse, d(2027, 12, 31)).is_none());
        assert!(!names(Exchange::Nyse, 2028).iter().any(|(_, n)| n == "New Year's Day"));
        // 1 Jan 2027 is a Friday.
        assert!(has(Exchange::Nyse, d(2027, 1, 1), DayKind::Closed));
    }

    #[test]
    fn juneteenth_and_floating_holidays() {
        assert!(has(Exchange::Nyse, d(2026, 6, 19), DayKind::Closed));
        assert!(has(Exchange::Nyse, d(2027, 6, 18), DayKind::Closed)); // Saturday → Friday
        assert!(has(Exchange::Nyse, d(2026, 5, 25), DayKind::Closed)); // Memorial Day
        assert!(has(Exchange::Nyse, d(2026, 9, 7), DayKind::Closed)); // Labor Day
        assert!(has(Exchange::Nyse, d(2026, 1, 19), DayKind::Closed)); // MLK
        assert!(has(Exchange::Nyse, d(2026, 2, 16), DayKind::Closed)); // Presidents' Day
    }

    #[test]
    fn nyse_2026_has_ten_closures() {
        let closed = holidays(Exchange::Nyse, 2026).iter().filter(|h| h.kind == DayKind::Closed).count();
        assert_eq!(closed, 10);
    }

    #[test]
    fn dst_offsets() {
        let off = |y, m, day, h| ny_offset_secs(utc(y, m, day, h, 0).naive_utc());
        // 2026: DST starts Sunday 8 March, ends Sunday 1 November.
        assert_eq!(off(2026, 3, 8, 6), -5 * 3600);
        assert_eq!(off(2026, 3, 8, 7), -4 * 3600);
        assert_eq!(off(2026, 11, 1, 5), -4 * 3600);
        assert_eq!(off(2026, 11, 1, 6), -5 * 3600);
        assert_eq!(off(2026, 7, 1, 12), -4 * 3600);
        assert_eq!(off(2026, 1, 15, 12), -5 * 3600);
    }

    #[test]
    fn nyse_sessions_follow_dst() {
        // Wednesday 14 Oct 2026 (EDT): 13:29 UTC = 09:29 ET, 13:30 UTC = 09:30 ET.
        assert_eq!(status(Exchange::Nyse, utc(2026, 10, 14, 13, 29)).phase, Phase::Pre);
        assert_eq!(status(Exchange::Nyse, utc(2026, 10, 14, 13, 30)).phase, Phase::Regular);
        assert_eq!(status(Exchange::Nyse, utc(2026, 10, 14, 19, 59)).phase, Phase::Regular);
        assert_eq!(status(Exchange::Nyse, utc(2026, 10, 14, 20, 0)).phase, Phase::After);
        assert_eq!(status(Exchange::Nyse, utc(2026, 10, 15, 0, 0)).phase, Phase::Closed);
        // Wednesday 13 Jan 2027 (EST): 14:30 UTC is the open.
        assert_eq!(status(Exchange::Nyse, utc(2027, 1, 13, 14, 29)).phase, Phase::Pre);
        assert_eq!(status(Exchange::Nyse, utc(2027, 1, 13, 14, 30)).phase, Phase::Regular);
        assert_eq!(status(Exchange::Nyse, utc(2027, 1, 13, 9, 0)).phase, Phase::Pre); // 04:00 ET
        assert_eq!(status(Exchange::Nyse, utc(2027, 1, 13, 8, 59)).phase, Phase::Closed);
    }

    #[test]
    fn nyse_early_close_day() {
        // Friday 27 Nov 2026: closes 13:00 EST = 18:00 UTC, after-hours until 17:00 ET.
        assert_eq!(status(Exchange::Nyse, utc(2026, 11, 27, 17, 59)).phase, Phase::Regular);
        assert_eq!(status(Exchange::Nyse, utc(2026, 11, 27, 18, 0)).phase, Phase::After);
        assert_eq!(status(Exchange::Nyse, utc(2026, 11, 27, 22, 0)).phase, Phase::Closed);
    }

    #[test]
    fn closed_days_and_weekends() {
        let s = status(Exchange::Nyse, utc(2026, 11, 26, 15, 0));
        assert_eq!(s.phase, Phase::Closed);
        assert!(s.note.contains("Thanksgiving"), "{}", s.note);
        assert!(s.note.contains("Fri Nov 27"), "{}", s.note);
        let w = status(Exchange::Nyse, utc(2026, 10, 17, 15, 0)); // Saturday
        assert_eq!(w.phase, Phase::Closed);
        assert!(w.note.contains("weekend") && w.note.contains("Mon Oct 19"), "{}", w.note);
    }

    #[test]
    fn local_date_can_differ_from_utc() {
        // 02:00 UTC on Thursday 15 Oct is still Wednesday evening in New York.
        let l = local_time(Exchange::Nyse, utc(2026, 10, 15, 2, 0));
        assert_eq!(l.date(), d(2026, 10, 14));
    }

    #[test]
    fn bist_fixed_holidays_and_weekends() {
        assert!(has(Exchange::Bist, d(2026, 10, 29), DayKind::Closed));
        assert!(has(Exchange::Bist, d(2026, 10, 28), DayKind::Early(t(13, 0))));
        assert!(has(Exchange::Bist, d(2027, 1, 1), DayKind::Closed));
        assert!(has(Exchange::Bist, d(2026, 7, 15), DayKind::Closed));
        // 1 May 2027 is a Saturday: nothing to list.
        assert!(day_info(Exchange::Bist, d(2027, 5, 1)).is_none());
    }

    #[test]
    fn bist_bayrams() {
        // 2026 (Borsa Istanbul table): 19 Mar half day, 20 Mar closed; 26 May half, 27-30 May closed.
        assert!(has(Exchange::Bist, d(2026, 3, 19), DayKind::Early(t(13, 0))));
        assert!(has(Exchange::Bist, d(2026, 3, 20), DayKind::Closed));
        assert!(has(Exchange::Bist, d(2026, 5, 26), DayKind::Early(t(13, 0))));
        assert!(has(Exchange::Bist, d(2026, 5, 29), DayKind::Closed));
        // 2027: Ramazan 9-11 Mar (Tue-Thu), Kurban 16-19 May (Sun-Wed); 15 May eve is a Saturday.
        assert!(has(Exchange::Bist, d(2027, 3, 8), DayKind::Early(t(13, 0))));
        assert!(has(Exchange::Bist, d(2027, 3, 10), DayKind::Closed));
        assert!(has(Exchange::Bist, d(2027, 5, 17), DayKind::Closed));
        assert!(has(Exchange::Bist, d(2027, 5, 19), DayKind::Closed)); // bayram and 19 May: once
        assert_eq!(holidays(Exchange::Bist, 2027).iter().filter(|h| h.date == d(2027, 5, 19)).count(), 1);
        assert!(day_info(Exchange::Bist, d(2027, 5, 15)).is_none());
        assert!(religious_known(2026) && religious_known(2027) && !religious_known(2028));
    }

    #[test]
    fn bist_sessions_use_istanbul_time() {
        // Wednesday 14 Oct 2026: 07:00 UTC = 10:00 TRT.
        assert_eq!(status(Exchange::Bist, utc(2026, 10, 14, 6, 41)).phase, Phase::Pre);
        assert_eq!(status(Exchange::Bist, utc(2026, 10, 14, 7, 0)).phase, Phase::Regular);
        assert_eq!(status(Exchange::Bist, utc(2026, 10, 14, 14, 59)).phase, Phase::Regular);
        assert_eq!(status(Exchange::Bist, utc(2026, 10, 14, 15, 0)).phase, Phase::Closed);
        // Half day 28 Oct: closes 13:00 TRT = 10:00 UTC.
        assert_eq!(status(Exchange::Bist, utc(2026, 10, 28, 9, 59)).phase, Phase::Regular);
        assert_eq!(status(Exchange::Bist, utc(2026, 10, 28, 10, 0)).phase, Phase::Closed);
        // The same instant in winter is still UTC+3.
        assert_eq!(status(Exchange::Bist, utc(2027, 1, 13, 7, 0)).phase, Phase::Regular);
    }

    #[test]
    fn upcoming_spans_into_next_year() {
        let u = upcoming(Exchange::Nyse, d(2026, 10, 16));
        assert_eq!(u.first().map(|h| h.date), Some(d(2026, 11, 26)));
        assert!(u.iter().any(|h| h.date == d(2027, 3, 26)));
        assert!(u.windows(2).all(|w| w[0].date <= w[1].date));
    }

    #[test]
    fn symbol_exchange() {
        assert_eq!(exchange_for_symbol("AAPL"), Exchange::Nyse);
        assert_eq!(exchange_for_symbol("thyao.is"), Exchange::Bist);
    }
}
