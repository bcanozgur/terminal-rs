//! Multi-source market news (`TOP`): public RSS feeds grouped into World, Türkiye, Crypto and
//! Central Banks tabs, merged newest-first and de-duplicated. Parsing is [`crate::rss`].

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::NewsItem;

/// A tab of the pane. `All` merges the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    All,
    World,
    Turkiye,
    Crypto,
    CentralBanks,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::All, Tab::World, Tab::Turkiye, Tab::Crypto, Tab::CentralBanks];

    pub fn label(self) -> &'static str {
        match self {
            Tab::All => "All",
            Tab::World => "World",
            Tab::Turkiye => "Türkiye",
            Tab::Crypto => "Crypto",
            Tab::CentralBanks => "Central Banks",
        }
    }

    /// Saved-layout / command word.
    pub fn id(self) -> &'static str {
        match self {
            Tab::All => "all",
            Tab::World => "world",
            Tab::Turkiye => "tr",
            Tab::Crypto => "crypto",
            Tab::CentralBanks => "cb",
        }
    }

    /// `TOP TR`, `TOP WORLD`, `TOP CRYPTO`, `TOP CB`; anything else is `All`.
    pub fn parse(s: &str) -> Tab {
        match s.trim().to_ascii_lowercase().as_str() {
            "world" | "w" | "global" | "us" => Tab::World,
            "tr" | "turkiye" | "türkiye" | "turkey" | "bist" => Tab::Turkiye,
            "crypto" | "c" | "btc" | "coin" => Tab::Crypto,
            "cb" | "cbs" | "central" | "banks" | "fed" | "ecb" => Tab::CentralBanks,
            _ => Tab::All,
        }
    }
}

/// One RSS source.
#[derive(Debug, Clone, Copy)]
pub struct Feed {
    pub name: &'static str,
    pub url: &'static str,
    pub tab: Tab,
}

pub const FEEDS: [Feed; 8] = [
    Feed { name: "CNBC", url: "https://www.cnbc.com/id/100003114/device/rss/rss.html", tab: Tab::World },
    Feed { name: "MarketWatch", url: "https://feeds.content.dowjones.io/public/rss/mw_topstories", tab: Tab::World },
    Feed { name: "Bloomberg HT", url: "https://www.bloomberght.com/rss/tum-haberler.xml", tab: Tab::Turkiye },
    Feed { name: "AA", url: "https://www.aa.com.tr/tr/rss/default?cat=ekonomi", tab: Tab::Turkiye },
    Feed { name: "CoinDesk", url: crate::rss::COINDESK, tab: Tab::Crypto },
    Feed { name: "The Block", url: "https://www.theblock.co/rss.xml", tab: Tab::Crypto },
    Feed { name: "Fed", url: "https://www.federalreserve.gov/feeds/press_all.xml", tab: Tab::CentralBanks },
    Feed { name: "ECB", url: "https://www.ecb.europa.eu/rss/press.html", tab: Tab::CentralBanks },
];

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) \
                  Version/17.0 Safari/605.1.15";

/// Largest feed read.
const MAX_BYTES: usize = 4 << 20;

/// A headline and the tab its feed belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct TopItem {
    pub item: NewsItem,
    pub tab: Tab,
}

/// Fetch one feed. The publisher is the feed's name (no per-author suffix).
pub async fn fetch_feed(feed: &Feed) -> Result<Vec<TopItem>> {
    let client = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(15)).build()?;
    let resp = client.get(feed.url).send().await.context("request failed")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    let bytes = resp.bytes().await.context("reading the feed")?;
    let xml = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]).into_owned();
    parse_feed(&xml, feed)
}

/// Parse a feed's XML into [`TopItem`]s (newest first).
pub fn parse_feed(xml: &str, feed: &Feed) -> Result<Vec<TopItem>> {
    let items = crate::rss::parse_rss(xml, feed.name)?;
    Ok(items
        .into_iter()
        .map(|mut item| {
            item.publisher = feed.name.to_string();
            TopItem { item, tab: feed.tab }
        })
        .collect())
}

fn norm_title(t: &str) -> String {
    t.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The link without scheme, `www.`, query, fragment and trailing slash, so one story linked
/// twice with different tracking parameters counts once.
fn norm_link(l: &str) -> String {
    let l = l.split(['?', '#']).next().unwrap_or(l);
    let l = l.strip_prefix("https://").or_else(|| l.strip_prefix("http://")).unwrap_or(l);
    l.strip_prefix("www.").unwrap_or(l).trim_end_matches('/').to_ascii_lowercase()
}

/// Newest first, one row per link and per normalised title (the first, newest, wins).
pub fn merge(mut items: Vec<TopItem>) -> Vec<TopItem> {
    items.sort_by_key(|a| std::cmp::Reverse(a.item.ts));
    let (mut links, mut titles) = (HashSet::new(), HashSet::new());
    items.retain(|t| {
        let (l, n) = (norm_link(&t.item.link), norm_title(&t.item.title));
        // Evaluate both so each set learns the row even when the other already knew it.
        let fresh_link = links.insert(l);
        let fresh_title = n.is_empty() || titles.insert(n);
        fresh_link && fresh_title
    });
    items
}

/// Rows for `tab`: [`Tab::All`] keeps everything.
pub fn for_tab(items: &[TopItem], tab: Tab) -> impl Iterator<Item = &TopItem> {
    items.iter().filter(move |t| tab == Tab::All || t.tab == tab)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(name: &str) -> Feed {
        *FEEDS.iter().find(|f| f.name == name).unwrap()
    }

    fn it(title: &str, link: &str, ts: i64, tab: Tab) -> TopItem {
        TopItem { item: NewsItem { title: title.into(), publisher: "x".into(), link: link.into(), ts, tickers: vec![] }, tab }
    }

    #[test]
    fn feed_list_covers_every_tab() {
        for tab in [Tab::World, Tab::Turkiye, Tab::Crypto, Tab::CentralBanks] {
            assert!(FEEDS.iter().filter(|f| f.tab == tab).count() >= 2, "{tab:?}");
        }
        assert!(FEEDS.iter().all(|f| f.url.starts_with("https://")));
    }

    #[test]
    fn tab_words() {
        assert_eq!(Tab::parse("TR"), Tab::Turkiye);
        assert_eq!(Tab::parse("world"), Tab::World);
        assert_eq!(Tab::parse("Crypto"), Tab::Crypto);
        assert_eq!(Tab::parse("cb"), Tab::CentralBanks);
        assert_eq!(Tab::parse(""), Tab::All);
        for t in Tab::ALL {
            assert_eq!(Tab::parse(t.id()), t);
        }
    }

    #[test]
    fn feed_fixtures_are_tagged_with_feed_and_tab() {
        let n = parse_feed(include_str!("../tests/fixtures/cnbc_rss.xml"), &feed("CNBC")).unwrap();
        assert!(n.iter().all(|t| t.tab == Tab::World && t.item.publisher == "CNBC"));
        // MarketWatch carries <dc:creator>; the publisher stays the feed name.
        let n = parse_feed(include_str!("../tests/fixtures/marketwatch_rss.xml"), &feed("MarketWatch")).unwrap();
        assert!(n.iter().all(|t| t.item.publisher == "MarketWatch"));
        let n = parse_feed(include_str!("../tests/fixtures/ecb_rss.xml"), &feed("ECB")).unwrap();
        assert!(n.iter().all(|t| t.tab == Tab::CentralBanks));
        assert!(parse_feed("<html/>", &feed("ECB")).is_err());
    }

    #[test]
    fn merge_sorts_and_dedups_by_link_and_title() {
        let merged = merge(vec![
            it("Fed holds rates", "https://www.a.com/1?utm=x", 100, Tab::World),
            it("Older story", "https://b.com/2", 50, Tab::World),
            it("Fed holds rates", "https://a.com/1/", 90, Tab::Crypto),
            it("FED HOLDS RATES!", "https://c.com/9", 80, Tab::Turkiye),
            it("Newest", "https://d.com/3", 200, Tab::Crypto),
        ]);
        let titles: Vec<_> = merged.iter().map(|t| t.item.title.as_str()).collect();
        assert_eq!(titles, ["Newest", "Fed holds rates", "Older story"]);
        assert_eq!(merged[1].item.ts, 100);
    }

    #[test]
    fn tab_filter() {
        let v = vec![it("a", "https://a/1", 3, Tab::World), it("b", "https://b/1", 2, Tab::Crypto)];
        assert_eq!(for_tab(&v, Tab::All).count(), 2);
        assert_eq!(for_tab(&v, Tab::Crypto).count(), 1);
    }
}
