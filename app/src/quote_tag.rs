//! Where a quote came from and how current it is: header tags, row markers, tooltips and the
//! sample-data notice. The rules live in [`tr_data::freshness`]; this only words and colours
//! them.

use std::collections::HashMap;

use chrono::{DateTime, Local, TimeZone};
use egui::Color32;
use tr_data::Quote;
use tr_data::freshness::{Health, QuoteSource, Timeliness, fmt_age, is_open, market_of};

use crate::theme::{self, amber, muted};

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A short label and its colour.
pub struct Tag {
    pub text: String,
    pub color: Color32,
}

fn local(ts: i64) -> DateTime<Local> {
    Local.timestamp_opt(ts, 0).single().unwrap_or_default()
}

/// `16:00` for today, `Fri 16:00` within the week, `12 Oct 16:00` beyond.
fn when(ts: i64, now: i64) -> String {
    let (t, n) = (local(ts), local(now));
    if t.date_naive() == n.date_naive() {
        t.format("%H:%M").to_string()
    } else if now - ts < 6 * 86_400 {
        t.format("%a %H:%M").to_string()
    } else {
        t.format("%d %b %H:%M").to_string()
    }
}

fn day(ts: i64) -> String {
    local(ts).format("%d %b").to_string()
}

/// `Yahoo · 15m delayed`, `Binance · live`, `Treasury.gov · end of day`.
pub fn source_text(q: &Quote) -> String {
    match q.timeliness() {
        Timeliness::Unknown => q.source.name().to_string(),
        Timeliness::Simulated => "simulated".into(),
        t => format!("{} · {}", q.source.name(), t.label()),
    }
}

/// The tag for a chart or pane header (`None` for quotes whose source was never recorded).
pub fn tag(q: &Quote, now: i64) -> Option<Tag> {
    let (text, color) = match q.health(now) {
        Health::Simulated => ("SAMPLE DATA".to_string(), theme::down()),
        Health::Stale { age } => (format!("stale {} · {}", fmt_age(age), q.source.name()), amber()),
        Health::Closed => {
            // A daily bar's time is its open, so without a trade time only the day is known.
            let at = if q.traded.is_some() { when(q.as_of(), now) } else { day(q.ts) };
            (format!("closed · as of {at}"), muted())
        }
        Health::EndOfDay => (format!("{} · as of {}", source_text(q), day(q.ts)), muted()),
        Health::Fresh if q.source == QuoteSource::Unknown => return None,
        Health::Fresh => (source_text(q), if q.timeliness() == Timeliness::Live { theme::up() } else { muted() }),
    };
    Some(Tag { text, color })
}

/// The row marker: nothing for live, current data; grey for delayed, closed or end-of-day;
/// amber for stale; red for sample data.
pub fn marker(q: &Quote, now: i64) -> Option<Color32> {
    match q.health(now) {
        Health::Simulated => Some(theme::down()),
        Health::Stale { .. } => Some(amber()),
        _ if q.source == QuoteSource::Unknown => None,
        Health::Fresh if q.timeliness() == Timeliness::Live => None,
        _ => Some(muted()),
    }
}

/// Multi-line hover text: source, delay and last update.
pub fn tooltip(q: &Quote, now: i64) -> String {
    let market = market_of(&q.symbol);
    let mut s = vec![format!("Source: {}", q.source.name())];
    let delay = q.timeliness().label();
    if !delay.is_empty() {
        s.push(format!("Delay: {delay}"));
    }
    match q.health(now) {
        Health::Simulated => {
            s.push(format!("Simulated prices; {} could not be reached.", QuoteSource::intended(&q.symbol).name()));
        }
        Health::EndOfDay => s.push(format!("Last close: {}", day(q.ts))),
        h => {
            if q.traded.is_some() {
                s.push(format!("Last trade: {} ({} ago)", when(q.as_of(), now), fmt_age(now - q.as_of())));
            } else {
                s.push(format!("Last bar: {}", day(q.ts)));
            }
            match h {
                Health::Stale { age } => s.push(format!("Stale: market open, no update for {}", fmt_age(age))),
                Health::Closed => s.push("Market closed".into()),
                _ if is_open(market, now) => s.push("Market open".into()),
                _ => {}
            }
        }
    }
    s.join("\n")
}

/// Sample quotes among `keys`, as the sources they stand in for (in first-seen order).
pub fn sample_sources<'a>(quotes: &HashMap<String, Quote>, keys: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        if let Some(q) = quotes.get(k).filter(|q| q.source == QuoteSource::Sample) {
            let n = QuoteSource::intended(&q.symbol).name().to_string();
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// One line per provider among `keys`: how many quotes, and their timeliness.
pub fn provider_summary<'a>(quotes: &HashMap<String, Quote>, keys: impl IntoIterator<Item = &'a String>) -> String {
    let mut rows: Vec<(String, usize)> = Vec::new();
    for q in keys.into_iter().filter_map(|k| quotes.get(k)) {
        let label = source_text(q);
        match rows.iter_mut().find(|(l, _)| *l == label) {
            Some(r) => r.1 += 1,
            None => rows.push((label, 1)),
        }
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.iter().map(|(l, n)| format!("{l}: {n} quote{}", if *n == 1 { "" } else { "s" })).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(sym: &str, source: QuoteSource, ts: i64, traded: Option<i64>) -> Quote {
        Quote { symbol: sym.into(), last: 1.0, prev_close: 1.0, ts, spark: vec![], extended: None, source, traded }
    }

    #[test]
    fn sample_notice_names_the_unreachable_source() {
        let mut m = HashMap::new();
        let keys: Vec<String> =
            ["AAPL US Equity", "BTCUSD Curncy", "US10Y Govt", "MSFT US Equity"].into_iter().map(String::from).collect();
        for k in &keys {
            m.insert(k.clone(), quote(k, QuoteSource::Sample, 0, None));
        }
        m.insert(keys[3].clone(), quote(&keys[3], QuoteSource::Yahoo, 0, None));
        assert_eq!(sample_sources(&m, &keys), vec!["Yahoo", "Binance", "Treasury.gov"]);
    }

    #[test]
    fn markers_are_quiet_for_live_data() {
        let now = 1_791_000_000; // crypto is open whatever the weekday
        let live = quote("BTCUSD Curncy", QuoteSource::Binance, now, Some(now));
        assert!(marker(&live, now).is_none());
        let stale = quote("BTCUSD Curncy", QuoteSource::Binance, now - 600, Some(now - 600));
        assert_eq!(marker(&stale, now), Some(amber()));
        assert_eq!(marker(&quote("BTCUSD Curncy", QuoteSource::Sample, now, None), now), Some(theme::down()));
        assert_eq!(tag(&live, now).unwrap().text, "Binance · live");
        assert_eq!(tag(&quote("BTCUSD Curncy", QuoteSource::Sample, now, None), now).unwrap().text, "SAMPLE DATA");
        assert!(tag(&stale, now).unwrap().text.starts_with("stale 10m"));
    }
}
