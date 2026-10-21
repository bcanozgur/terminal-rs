//! Minimal RSS 2.0 reader for headline feeds (title, link, pubDate). Used for crypto
//! news, which Yahoo's search endpoint does not index.

use anyhow::{Result, bail};

use crate::NewsItem;

/// CoinDesk's public feed.
pub const COINDESK: &str = "https://www.coindesk.com/arc/outboundfeeds/rss/";

/// First `<name ...>text</name>` in `item`; the name must end at `>`, whitespace or `/` so that
/// `<link` does not match `<linkedin>`.
fn tag<'a>(item: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let mut from = 0;
    let start = loop {
        let at = from + item[from..].find(&open)?;
        match item[at + open.len()..].chars().next() {
            Some(c) if c == '>' || c == '/' || c.is_whitespace() => break at,
            _ => from = at + open.len(),
        }
    };
    let body = &item[start + open.len()..];
    // Skip attributes; a self-closing tag has no text.
    let gt = body.find('>')?;
    if body[..gt].ends_with('/') {
        return None;
    }
    let body = &body[gt + 1..];
    let end = body.find(&format!("</{name}>"))?;
    Some(body[..end].trim())
}

/// Decode `&#8217;` / `&#x2019;` at the start of `s` (after the `&`): the char and the length consumed.
fn numeric_entity(s: &str) -> Option<(char, usize)> {
    let rest = s.strip_prefix('#')?;
    let semi = rest.find(';').filter(|&i| i <= 8)?;
    let num = &rest[..semi];
    let code = match num.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => num.parse().ok()?,
    };
    Some((char::from_u32(code)?, semi + 2))
}

fn text(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.strip_prefix("<![CDATA[").and_then(|r| r.strip_suffix("]]>")).unwrap_or(raw);
    // One pass, so `&amp;lt;` decodes to `&lt;` and not `<`.
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some((c, n)) = numeric_entity(rest) {
            out.push(c);
            rest = &rest[n..];
            continue;
        }
        let named = [("lt;", '<'), ("gt;", '>'), ("quot;", '"'), ("apos;", '\''), ("amp;", '&'), ("nbsp;", ' ')];
        match named.iter().find(|(n, _)| rest.starts_with(n)) {
            Some((n, c)) => {
                out.push(*c);
                rest = &rest[n.len()..];
            }
            None => out.push('&'),
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Publication time (Unix seconds): RFC 2822 with or without seconds, or RFC 3339.
fn parse_date(raw: &str) -> Option<i64> {
    let s = text(raw);
    if let Ok(d) = chrono::DateTime::parse_from_rfc2822(&s).or_else(|_| chrono::DateTime::parse_from_rfc3339(&s)) {
        return Some(d.timestamp());
    }
    // `Wed, 21 Oct 2026 21:15 GMT` (CNBC): no seconds.
    let s = s.replace(" GMT", " +0000").replace(" UTC", " +0000").replace(" UT", " +0000");
    for f in ["%a, %d %b %Y %H:%M %z", "%d %b %Y %H:%M:%S %z", "%d %b %Y %H:%M %z"] {
        if let Ok(d) = chrono::DateTime::parse_from_str(&s, f) {
            return Some(d.timestamp());
        }
    }
    None
}

/// The `<item>` bodies of a feed: `<item>` or `<item rdf:about="...">`, but not `<itemfoo>`.
fn items(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<item") {
        let after = &rest[i + 5..];
        if !after.starts_with(['>', ' ', '\t', '\n', '\r']) {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        let body = &after[gt + 1..];
        let end = body.find("</item>").unwrap_or(body.len());
        out.push(&body[..end]);
        rest = &body[end..];
    }
    out
}

/// Parse `<item>`s into headlines (newest first), tagging the feed's publisher.
pub fn parse_rss(xml: &str, publisher: &str) -> Result<Vec<NewsItem>> {
    if !xml.contains("<rss") && !xml.contains("<channel") {
        bail!("not an RSS feed");
    }
    let mut out = Vec::new();
    for item in items(xml) {
        let (Some(title), Some(link)) = (tag(item, "title"), tag(item, "link")) else { continue };
        let (title, link) = (text(title), text(link));
        if title.is_empty() || link.is_empty() {
            continue;
        }
        let ts = tag(item, "pubDate").or_else(|| tag(item, "dc:date")).and_then(parse_date).unwrap_or(0);
        out.push(NewsItem {
            title,
            publisher: tag(item, "dc:creator")
                .map(text)
                .filter(|s| !s.is_empty())
                .map_or_else(|| publisher.to_string(), |a| format!("{publisher} · {a}")),
            link,
            ts,
            tickers: Vec::new(),
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.ts));
    Ok(out)
}

/// Keep headlines mentioning any of `words` (case-insensitive); all of them if none match.
pub fn filter_relevant(items: Vec<NewsItem>, words: &[&str]) -> Vec<NewsItem> {
    let hit = |n: &NewsItem| {
        let t = n.title.to_lowercase();
        words.iter().any(|w| t.contains(&w.to_lowercase()))
    };
    if items.iter().any(hit) { items.into_iter().filter(hit).collect() } else { items }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_coindesk_fixture() {
        let n = parse_rss(include_str!("../tests/fixtures/coindesk_rss.xml"), "CoinDesk").unwrap();
        assert_eq!(n.len(), 5);
        assert!(n.iter().all(|x| x.link.starts_with("https://www.coindesk.com/") && x.ts > 1_700_000_000));
        assert!(n.iter().all(|x| !x.title.contains("CDATA") && x.publisher.starts_with("CoinDesk")));
        assert!(n.windows(2).all(|w| w[0].ts >= w[1].ts));
        assert!(parse_rss("<html></html>", "x").is_err());
    }

    #[test]
    fn entities_and_filtering() {
        let xml = "<rss><channel><item><title>Bitcoin &amp; Ether rally</title><link>https://a/1</link>\
                   <pubDate>Mon, 12 Oct 2026 12:09:09 +0000</pubDate></item><item><title>Fed holds</title>\
                   <link>https://a/2</link></item></channel></rss>";
        let n = parse_rss(xml, "T").unwrap();
        assert_eq!(n[0].title, "Bitcoin & Ether rally");
        assert_eq!(filter_relevant(n.clone(), &["bitcoin"]).len(), 1);
        assert_eq!(filter_relevant(n, &["solana"]).len(), 2);
    }

    fn fx(xml: &str, who: &str) -> Vec<NewsItem> {
        let n = parse_rss(xml, who).unwrap();
        assert_eq!(n.len(), 5, "{who}");
        assert!(
            n.iter().all(|x| !x.title.is_empty() && x.link.starts_with("http") && x.ts > 1_700_000_000),
            "{who}: {n:?}"
        );
        assert!(n.iter().all(|x| !x.title.contains("CDATA") && !x.title.contains("&#") && x.title == x.title.trim()));
        assert!(n.windows(2).all(|w| w[0].ts >= w[1].ts));
        n
    }

    #[test]
    fn parses_cnbc_fixture() {
        // `<pubDate>Wed, 21 Oct 2026 21:15 GMT</pubDate>` has no seconds.
        let n = fx(include_str!("../tests/fixtures/cnbc_rss.xml"), "CNBC");
        assert!(n[0].link.starts_with("https://www.cnbc.com/"));
    }

    #[test]
    fn parses_marketwatch_fixture_with_numeric_entities() {
        let n = fx(include_str!("../tests/fixtures/marketwatch_rss.xml"), "MarketWatch");
        assert!(n.iter().any(|x| x.title.contains('\u{2014}') || x.title.contains('\u{2019}')), "{n:?}");
    }

    #[test]
    fn parses_bloomberght_fixture() {
        let n = fx(include_str!("../tests/fixtures/bloomberght_rss.xml"), "Bloomberg HT");
        assert!(n.iter().any(|x| !x.title.is_ascii()));
    }

    #[test]
    fn parses_aa_fixture_with_bom_and_attributes() {
        let n = fx(include_str!("../tests/fixtures/aa_rss.xml"), "AA");
        assert!(n.iter().all(|x| x.link.starts_with("https://www.aa.com.tr/")));
    }

    #[test]
    fn parses_fed_fixture_with_cdata_dates() {
        fx(include_str!("../tests/fixtures/fed_rss.xml"), "Fed");
    }

    #[test]
    fn parses_ecb_fixture_single_line_with_padded_titles() {
        let n = fx(include_str!("../tests/fixtures/ecb_rss.xml"), "ECB");
        assert!(n[0].link.starts_with("https://www.ecb.europa.eu/"));
    }

    #[test]
    fn parses_theblock_fixture() {
        fx(include_str!("../tests/fixtures/theblock_rss.xml"), "The Block");
    }

    #[test]
    fn item_attributes_numeric_entities_and_date_variants() {
        let xml = "<rss><channel><itemize>x</itemize><item rdf:about=\"a\"><title>It&#8217;s &#x2014; &amp;lt; ok</title>\
                   <link>https://a/1</link><pubDate>Wed, 21 Oct 2026 21:15 GMT</pubDate></item>\
                   <item><title>B</title><link>https://a/2</link><dc:date>2026-10-21T21:16:00Z</dc:date></item>\
                   <item><title>C</title><link/></item></channel></rss>";
        let n = parse_rss(xml, "T").unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(n[1].title, "It\u{2019}s \u{2014} &lt; ok");
        assert_eq!(n[1].ts, 1_792_617_300);
        assert_eq!(n[0].ts, 1_792_617_360);
    }
}
