//! Readable text of a news article, for reading a headline inside the News pane.
//!
//! Yahoo's headline links point at finance.yahoo.com article pages (syndicated publishers),
//! which serve the story as plain HTML. The page is fetched like a browser would and reduced to
//! its headings and paragraphs; pay-walled or script-rendered pages come back with little or no
//! text and the pane falls back to "Open in Browser".

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// One block of an article, in reading order.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading(String),
    Paragraph(String),
    Bullet(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Article {
    pub title: String,
    pub blocks: Vec<Block>,
}

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) \
                  Version/17.0 Safari/605.1.15";

/// Largest page read; pages are around 1 MB of mostly script.
const MAX_BYTES: usize = 4 << 20;

/// Fetch `url` and extract its article text.
pub async fn fetch(url: &str) -> Result<Article> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("not a web link");
    }
    let client = reqwest::Client::builder().user_agent(UA).timeout(Duration::from_secs(20)).build()?;
    let resp = client.get(url).send().await.context("fetching the article")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("the publisher answered HTTP {status}");
    }
    let bytes = resp.bytes().await.context("reading the article")?;
    let html = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]).into_owned();
    let art = parse(&html);
    if art.blocks.iter().filter(|b| matches!(b, Block::Paragraph(_))).count() < 2 {
        bail!("the page has no readable text (pay-walled or script-rendered)");
    }
    Ok(art)
}

/// Remove `<tag ...>...</tag>` blocks (case-insensitive) from `html`.
fn strip_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while let Some(i) = lower[at..].find(&open) {
        let start = at + i;
        // `<p` must not match `<picture`: the tag name ends at whitespace, `>` or `/`.
        let after = lower.as_bytes().get(start + open.len()).copied().unwrap_or(b'>');
        if !matches!(after, b' ' | b'>' | b'/' | b'\n' | b'\t' | b'\r') {
            out.push_str(&html[at..start + open.len()]);
            at = start + open.len();
            continue;
        }
        out.push_str(&html[at..start]);
        match lower[start..].find(&close) {
            Some(j) => at = start + j + close.len(),
            None => {
                at = html.len();
                break;
            }
        }
    }
    out.push_str(&html[at..]);
    out
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" | "#x27" => Some('\''),
            "nbsp" => Some(' '),
            "ndash" => Some('–'),
            "mdash" => Some('—'),
            "lsquo" => Some('‘'),
            "rsquo" => Some('’'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            "hellip" => Some('…'),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Visible text of an HTML fragment, whitespace collapsed.
fn text_of(fragment: &str) -> String {
    let mut plain = String::with_capacity(fragment.len());
    let mut in_tag = false;
    for c in fragment.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                plain.push(' ');
            }
            _ if !in_tag => plain.push(c),
            _ => {}
        }
    }
    let plain = decode_entities(&plain);
    let mut out = String::new();
    for word in plain.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    // Tags were replaced by a space; take that back before punctuation ("word ." -> "word.").
    out.replace(" .", ".").replace(" ,", ",").replace(" ;", ";").replace(" ’", "’")
}

/// Navigation and promo lines that sit inside article markup.
const BOILERPLATE: &[&str] = &[
    "story continues",
    "advertisement",
    "read more",
    "sign up",
    "subscribe",
    "follow us",
    "click here",
    "just released. our analysts",
    "newsletter",
    "terms and privacy",
];

/// Promotional paragraphs, dropped at any length.
const PROMO: &[&str] = &["our analysts combed", "enter your email", "the ten best stocks to buy"];

fn is_boilerplate(s: &str) -> bool {
    let l = s.to_lowercase();
    (l.chars().count() < 80 && BOILERPLATE.iter().any(|b| l.contains(b))) || PROMO.iter().any(|b| l.contains(b))
}

/// `content` of `<meta property="og:title" ...>`.
fn og_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let i = lower.find("og:title")?;
    let tag_start = lower[..i].rfind('<')?;
    let tag_end = i + lower[i..].find('>')?;
    let tag = &html[tag_start..tag_end];
    let c = tag.to_ascii_lowercase().find("content=")? + 8;
    let q = tag[c..].chars().next().filter(|q| *q == '"' || *q == '\'')?;
    let rest = &tag[c + 1..];
    Some(text_of(&rest[..rest.find(q)?]))
}

/// Reduce an article page to its headings, paragraphs and list items.
pub fn parse(html: &str) -> Article {
    let lower = html.to_ascii_lowercase();
    // The page's first <h1> is often the site logo, so the social title comes first.
    let title = og_title(html)
        .filter(|t| !t.is_empty())
        .or_else(|| {
            let i = lower.find("<h1")?;
            let body = &html[i..];
            let s = body.find('>')? + 1;
            let e = body.to_ascii_lowercase().find("</h1>")?;
            Some(text_of(&body[s..e]))
        })
        .or_else(|| {
            let i = lower.find("<title>")? + 7;
            let e = lower[i..].find("</title>")?;
            Some(text_of(&html[i..i + e]))
        })
        .unwrap_or_default();

    // The story lives in <article>; other pages fall back to the whole body.
    let start = lower.find("<article").unwrap_or_else(|| lower.find("<body").unwrap_or(0));
    let end = lower.rfind("</article>").map_or(html.len(), |e| e + 10);
    let mut body = html[start..end.max(start)].to_string();
    for tag in ["script", "style", "svg", "noscript", "button", "nav", "aside", "form", "footer", "figure"] {
        body = strip_blocks(&body, tag);
    }

    let lower = body.to_ascii_lowercase();
    let mut blocks: Vec<Block> = Vec::new();
    let mut at = 0;
    while let Some(i) = lower[at..].find('<') {
        let tag_start = at + i;
        let name: String = lower[tag_start + 1..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>();
        let make: Option<fn(String) -> Block> = match name.as_str() {
            "p" => Some(Block::Paragraph),
            "li" => Some(Block::Bullet),
            "h2" | "h3" | "h4" => Some(Block::Heading),
            _ => None,
        };
        let Some(make) = make else {
            at = tag_start + 1;
            continue;
        };
        let Some(gt) = lower[tag_start..].find('>') else { break };
        let inner_start = tag_start + gt + 1;
        let close = format!("</{name}>");
        let Some(j) = lower[inner_start..].find(&close) else {
            at = inner_start;
            continue;
        };
        let txt = text_of(&body[inner_start..inner_start + j]);
        at = inner_start + j + close.len();
        let long_enough = match name.as_str() {
            "p" | "li" => txt.chars().count() >= 25,
            _ => txt.chars().count() >= 3,
        };
        if !long_enough || is_boilerplate(&txt) {
            continue;
        }
        // Yahoo puts "<title> <author> <date>" in the first paragraph.
        if !title.is_empty() && txt.starts_with(&title) {
            continue;
        }
        let b = make(txt);
        if blocks.last() != Some(&b) {
            blocks.push(b);
        }
    }
    Article { title, blocks }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>Ignored | Yahoo Finance</title>
        <script>var x = "<p>not a paragraph in script</p>";</script></head><body>
        <nav><p>Menu item that is long enough to be kept otherwise</p></nav>
        <h1 class="hdr">S&amp;P 500 Hits a Record</h1>
        <article>
          <p>S&amp;P 500 Hits a Record   Jane Doe   Mon, Oct 12 at 4:05 PM</p>
          <p>Stocks rose on <a href="/x">Monday</a> as investors weighed earnings&nbsp;and rates.</p>
          <figure><p>Photo caption that should be ignored entirely here</p></figure>
          <h2>What happened</h2>
          <p>Story Continues</p>
          <p>The benchmark index gained 1.2%&mdash;its best day since March&#8212;while bonds slipped.</p>
          <ul><li>Tech led the gains with a 2% advance on the day</li></ul>
          <p>Short.</p>
          <p>The benchmark index gained 1.2%—its best day since March—while bonds slipped.</p>
        </article></body></html>"#;

    #[test]
    fn extracts_headings_paragraphs_and_bullets() {
        let a = parse(PAGE);
        assert_eq!(a.title, "S&P 500 Hits a Record");
        assert_eq!(
            a.blocks,
            vec![
                Block::Paragraph("Stocks rose on Monday as investors weighed earnings and rates.".into()),
                Block::Heading("What happened".into()),
                Block::Paragraph(
                    "The benchmark index gained 1.2%—its best day since March—while bonds slipped.".into()
                ),
                Block::Bullet("Tech led the gains with a 2% advance on the day".into()),
                Block::Paragraph(
                    "The benchmark index gained 1.2%—its best day since March—while bonds slipped.".into()
                ),
            ]
        );
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &#8212; &#x2019; &bogus; & &"), "a & b <c> — ’ &bogus; & &");
    }

    #[test]
    fn strip_blocks_keeps_similar_tag_names() {
        assert_eq!(strip_blocks("<p>a</p><picture>b</picture>", "p"), "<picture>b</picture>");
    }

    #[test]
    fn page_without_article_falls_back_to_body() {
        let a = parse("<html><body><p>A paragraph long enough to be read as text.</p></body></html>");
        assert_eq!(a.blocks, vec![Block::Paragraph("A paragraph long enough to be read as text.".into())]);
    }
}
