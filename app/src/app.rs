//! Top-level terminal: command line, docked panes, saved layouts, hotkeys and status bar.
//!
//! Submodules: `chrome` (toolbar, sidebar, status bar), `workspace` (dock rendering,
//! drag and drop, pop-out windows, link groups, saved layouts), `panels` (per-kind pane
//! content) and `settings_view` (preferences UI).

mod broker;
mod changelog_view;
mod chrome;
mod commands;
mod demo;
mod des_view;
mod earnings;
mod eco_view;
mod edgar_view;
mod etf_view;
mod fa_view;
mod fut_view;
mod hotkeys_view;
mod input;
mod most_view;
mod options_view;
mod palette_view;
mod panels;
mod portfolio_view;
mod responses;
mod risk_view;
mod safety_view;
mod settings_view;
mod smart_view;
mod theming;
mod trading;
mod top_view;
mod trfunds_view;
mod wlcols_view;
mod workspace;

pub use broker::PosSort;

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use egui::{Color32, Event, Key, Modifiers, Rect, Ui, UiBuilder, pos2, vec2};
use serde::{Deserialize, Serialize};
use tr_chart::drawings::Tool;
use tr_congress::Trade;
use tr_congress::db::{Alert, PoliticianSummary};
use tr_data::{NewsItem, OrderBook, Quote, SearchHit, YieldCurves};

use crate::anim;
use crate::chart_view::ChartState;
use crate::command::{self, Command, Function};
use crate::congress_view::{Action, FeedFilter};
use crate::dock::{Node, PaneId};
use crate::earnings_view::{self, ErnPane};
use crate::layout::Layout;
use crate::layouts::{LayoutDoc, Link, PaneSettings, Store, WindowGeometry};
use crate::markets_view;
use crate::prefs::{self, Prefs};
use crate::theme;
use crate::worker::{Profile, Request, Response, Worker, data_dir};

use chrome::{SIDEBAR_W, STATUS_H, SidebarCmd};
use settings_view::SettingsTab;

pub enum PaneKind {
    Chart(Box<ChartState>),
    Feed {
        filter: FeedFilter,
    },
    Ticker {
        symbol: String,
        trades: Vec<Trade>,
        loading: bool,
    },
    Profile {
        query: String,
        data: Option<Box<Profile>>,
        loading: bool,
    },
    Politicians,
    Alerts,
    Watchlist,
    /// A named sidebar list (see [`WatchGroup`]) shown as a pane.
    List(String),
    Help,
    Changelog,
    Wei,
    Curve,
    Search {
        query: String,
    },
    News(Box<NewsPane>),
    OrderBook(Box<BookPane>),
    Options(Box<OptionsPane>),
    /// Brokerage accounts through SnapTrade (`PORT`, `ACCT`, `POS`, `ORD`, `ACTV`).
    Portfolio(Box<broker::PortfolioPane>),
    Settings,
    /// SEC EDGAR security description (`DES`) or financial analysis (`FA`).
    Edgar(Box<edgar_view::EdgarPane>),
    /// Earnings calendar (`ERN`) or one ticker's history (`AAPL ERN`).
    Earnings(Box<ErnPane>),
    /// US economic calendar (`ECO`) and release history (`ECOF`).
    Eco(Box<eco_view::EcoPane>),
    /// US ETF contents (`ETF`): holdings, sectors, reverse search and overlap.
    Etf(Box<etf_view::EtfPane>),
    /// Turkish mutual funds (`TRF`): KAP portfolio reports, allocation, buys/sells, reverse search.
    TrFunds(Box<trfunds_view::TrfPane>),
    /// US market movers (`MOST`): day gainers, losers and most active from Yahoo screeners.
    Most(Box<most_view::MostPane>),
    /// Market news from several RSS feeds (`TOP`).
    Top(Box<top_view::TopPane>),
    /// Futures curve, VIX term structure and sector heatmap (`FUT`, `VIX`, `HEAT`).
    Fut(Box<fut_view::FutPane>),
    /// Smart money (`SMART`): insiders (Form 4), institutions (13F) and short interest.
    Smart(Box<smart_view::SmartPane>),
    /// Portfolio risk (`RISK`, `CORR`): VaR, beta, drawdown, risk contribution, correlation heatmap.
    Risk(Box<risk_view::RiskPane>),
    /// Audit log of order attempts and safety changes, and market holidays (`AUDIT`, `HOL`).
    Audit(Box<safety_view::AuditPane>),
}

pub struct OptionsPane {
    pub symbol: String,
    /// Selected expiration (`None` = nearest).
    pub expiration: Option<i64>,
    pub chain: Option<Box<tr_data::OptionChain>>,
    /// Greeks per call and put, aligned with the chain's contracts.
    pub call_greeks: Vec<Option<tr_core::options::Greeks>>,
    pub put_greeks: Vec<Option<tr_core::options::Greeks>>,
    /// Risk-free rate used for the greeks.
    pub rate: f64,
    pub error: Option<String>,
    pub loading: bool,
    pub fetched: Option<Instant>,
    /// Strikes each side of the money (0 = all).
    pub strikes: usize,
    pub show_greeks: bool,
    /// Text being typed in the symbol box (`None` = showing the pane's own symbol).
    pub symbol_edit: Option<String>,
    /// Analytics and strategy tabs: loaded expirations and the strategy legs.
    pub an: options_view::analytics::AnState,
}

impl OptionsPane {
    pub fn new(symbol: &str, strikes: usize, show_greeks: bool) -> Self {
        Self {
            symbol: symbol.into(),
            expiration: None,
            chain: None,
            call_greeks: Vec::new(),
            put_greeks: Vec::new(),
            rate: 0.04,
            error: None,
            loading: true,
            fetched: None,
            strikes,
            show_greeks,
            symbol_edit: None,
            an: Default::default(),
        }
    }

    /// The expiration to request next: a queued one for the analytics tab, else the selected one.
    pub fn next_expiration(&mut self) -> Option<i64> {
        self.an.queue.pop().or(self.expiration)
    }

    /// Store a chain and derive greeks from each contract's implied volatility.
    pub fn set_chain(&mut self, mut chain: Box<tr_data::OptionChain>, rate: f64, now: i64) {
        // Expiry at the 16:00 New York close (≈ 20:00 UTC) of the expiration date.
        let t = ((chain.expiration + 20 * 3600 - now) as f64 / (365.0 * 86_400.0)).max(0.5 / 365.0);
        // Yahoo keeps listing a same-day expiry after the close; the nearest one still trading
        // is the useful default.
        if self.expiration.is_none()
            && chain.expiration + 20 * 3600 <= now
            && let Some(&live) = chain.expirations.iter().find(|&&e| e + 20 * 3600 > now)
        {
            self.expiration = Some(live);
            self.fetched = None;
            return;
        }
        solve_missing_ivs(&mut chain, t, rate);
        self.an.cache.insert(chain.expiration, chain.clone());
        if !self.an.queue.is_empty() {
            self.fetched = None; // load the next queued expiration right away
        }
        // A background load for the analytics tab must not replace the chain on screen.
        if self.expiration.is_some_and(|e| e != chain.expiration) && self.chain.is_some() {
            if self.chain.as_ref().is_some_and(|c| Some(c.expiration) != self.expiration) {
                self.fetched = None; // the selected expiration is still to come
            }
            return;
        }
        let g = |call: bool, c: &tr_data::OptionContract| {
            tr_core::options::black_scholes(call, chain.spot, c.strike, t, rate, chain.dividend_yield, c.iv?)
                .map(|(_, g)| g)
        };
        self.call_greeks = chain.calls.iter().map(|c| g(true, c)).collect();
        self.put_greeks = chain.puts.iter().map(|c| g(false, c)).collect();
        self.rate = rate;
        if self.expiration.is_none() || !chain.expirations.contains(&chain.expiration) {
            self.expiration = Some(chain.expiration);
        }
        self.chain = Some(chain);
        self.error = None;
        self.loading = false;
    }
}

/// Fill in implied volatilities the source left out (Yahoo's placeholders are dropped when the
/// chain is parsed) from each contract's own price: the bid/ask midpoint when both sides are
/// quoted, else the last trade. Contracts whose price no volatility explains stay without one.
fn solve_missing_ivs(chain: &mut tr_data::OptionChain, t: f64, rate: f64) {
    let (spot, q) = (chain.spot, chain.dividend_yield);
    for (call, side) in [(true, &mut chain.calls), (false, &mut chain.puts)] {
        for c in side.iter_mut().filter(|c| c.iv.is_none()) {
            let price = match (c.bid, c.ask) {
                (Some(b), Some(a)) if b > 0.0 && a >= b => Some((a + b) / 2.0),
                _ => c.last.filter(|_| c.volume.is_some_and(|v| v > 0)),
            };
            if let Some(iv) = price.and_then(|p| tr_core::options::implied_vol(call, spot, c.strike, t, rate, q, p)) {
                c.iv = Some(iv);
                c.iv_solved = true;
            }
        }
    }
}

pub struct NewsPane {
    /// `None` = general market headlines.
    pub symbol: Option<String>,
    pub items: Vec<NewsItem>,
    pub error: Option<String>,
    pub loading: bool,
    pub fetched: Option<Instant>,
    /// The headline open for reading in place of the list (not saved with the layout).
    pub reading: Option<Reading>,
    /// Link of the article last requested from the worker.
    pub read_requested: Option<String>,
}

/// An article opened inside the News pane.
pub struct Reading {
    pub item: NewsItem,
    pub state: ReadState,
}

pub enum ReadState {
    Loading,
    Ready(tr_data::article::Article),
    Failed(String),
}

impl NewsPane {
    pub fn new(symbol: Option<String>) -> Self {
        Self {
            symbol,
            items: Vec::new(),
            error: None,
            loading: true,
            fetched: None,
            reading: None,
            read_requested: None,
        }
    }
}

pub struct BookPane {
    pub symbol: String,
    pub depth: usize,
    /// Price bucket; 0 = raw levels.
    pub group: f64,
    pub book: Option<OrderBook>,
    pub error: Option<String>,
    pub fetched: Option<Instant>,
}

impl BookPane {
    pub fn new(symbol: &str, depth: usize, group: f64) -> Self {
        Self { symbol: symbol.into(), depth, group, book: None, error: None, fetched: None }
    }
}

impl PaneKind {
    /// Short tab title.
    fn tab_title(&self) -> String {
        let short = |s: &str| tr_data::security(s).ticker;
        match self {
            PaneKind::Chart(c) => short(&c.symbol),
            PaneKind::Feed { .. } => "Congress".into(),
            PaneKind::Ticker { symbol, .. } => format!("{} Congress", short(symbol)),
            PaneKind::Profile { query, data, .. } => {
                data.as_ref().map_or_else(|| query.clone(), |p| p.politician.name.clone())
            }
            PaneKind::Politicians => "Politicians".into(),
            PaneKind::Alerts => "Alerts".into(),
            PaneKind::Watchlist => "Watchlist".into(),
            PaneKind::List(name) => name.clone(),
            PaneKind::Help => "Help".into(),
            PaneKind::Changelog => "What's New".into(),
            PaneKind::Wei => "World Markets".into(),
            PaneKind::Curve => "Yield Curve".into(),
            PaneKind::Search { query } if query.is_empty() => "Search".into(),
            PaneKind::Search { query } => format!("Search: {query}"),
            PaneKind::News(n) => match &n.symbol {
                Some(s) => format!("{} News", short(s)),
                None => "Top News".into(),
            },
            PaneKind::OrderBook(b) => format!("{} Depth", short(&b.symbol)),
            PaneKind::Options(o) => format!("{} Options", short(&o.symbol)),
            PaneKind::Portfolio(p) => p.title(),
            PaneKind::Settings => "Settings".into(),
            PaneKind::Edgar(e) => e.title(),
            PaneKind::Earnings(e) => match &e.symbol {
                Some(s) => format!("{} Earnings", short(s)),
                None => "Earnings".into(),
            },
            PaneKind::Eco(e) => e.title(),
            PaneKind::Etf(e) => e.title(),
            PaneKind::TrFunds(e) => e.title(),
            PaneKind::Most(e) => e.title(),
            PaneKind::Top(e) => e.title(),
            PaneKind::Fut(e) => e.title(),
            PaneKind::Smart(e) => e.title(),
            PaneKind::Risk(e) => e.title(),
            PaneKind::Audit(e) => e.title(),
        }
    }

    /// The Bloomberg-style function code shown on the tab.
    fn code(&self) -> &'static str {
        match self {
            PaneKind::Chart(_) => "GP",
            PaneKind::Feed { .. } | PaneKind::Ticker { .. } => "CONG",
            PaneKind::Profile { .. } | PaneKind::Politicians => "POL",
            PaneKind::Alerts => "ALRT",
            PaneKind::Watchlist => "WL",
            PaneKind::List(_) => "LST",
            PaneKind::Help => "HELP",
            PaneKind::Changelog => "CHANGES",
            PaneKind::Wei => "WEI",
            PaneKind::Curve => "GC",
            PaneKind::Search { .. } => "SECF",
            PaneKind::News(_) => "CN",
            PaneKind::OrderBook(_) => "OB",
            PaneKind::Options(_) => "OMON",
            PaneKind::Portfolio(p) => p.view.code(),
            PaneKind::Settings => "PDF",
            PaneKind::Edgar(e) => e.code(),
            PaneKind::Earnings(_) => "ERN",
            PaneKind::Eco(e) => e.code(),
            PaneKind::Etf(e) => e.code(),
            PaneKind::TrFunds(e) => e.code(),
            PaneKind::Most(e) => e.code(),
            PaneKind::Top(e) => e.code(),
            PaneKind::Fut(e) => e.code(),
            PaneKind::Smart(e) => e.code(),
            PaneKind::Risk(e) => e.code(),
            PaneKind::Audit(e) => e.code(),
        }
    }

    /// What the panel shows and how to use it (the ⓘ button in the tab strip).
    fn about(&self) -> &'static str {
        match self {
            PaneKind::Chart(_) => {
                "Chart (GP): price bars with studies. Pick the interval and style in the header, right-click \
                 for studies, markers and drawing tools. Type a ticker to load it. The tag after the change names the \
                 price source and its delay (Binance · live, Yahoo · 15m delayed); it turns amber when an open \
                 market has gone quiet (stale), grey when the market is closed, and red for SAMPLE DATA."
            }
            PaneKind::Feed { .. } => {
                "Congress feed (CONG): stock trades disclosed by members of the US Congress (Senate eFD and \
                 House Clerk filings), newest first. Filter by party, chamber or side; click a row for details."
            }
            PaneKind::Ticker { .. } => "Congress trades in one security (TICKER CONG): who bought or sold it and when.",
            PaneKind::Profile { .. } => "Politician profile (POL): one member's disclosed trades and holdings.",
            PaneKind::Politicians => {
                "Politicians (POL): members of Congress ranked by disclosed trading; star one to track it."
            }
            PaneKind::Alerts => {
                "Alerts (ALRT): your price and indicator alerts, plus new Congress filings for tracked members \
                 and tickers. Add one with e.g. AAPL ALRT > 250 or from a chart's right-click menu."
            }
            PaneKind::Watchlist => {
                "Watchlist (WL): your securities with quotes. WL ADD <ticker> to add; drag to reorder. A dot beside a \
                 ticker marks delayed or end-of-day (grey), stale (amber) or sample (red) prices; hover for the \
                 source, delay and last update."
            }
            PaneKind::List(_) => {
                "Security list (LST): a ready-made group such as BIST 30, sorted by volume. A dot beside a ticker marks \
                 delayed, stale (amber) or sample (red) prices; hover for the source and delay."
            }
            PaneKind::Help => "Commands and hotkeys (HELP): every function code and keyboard shortcut.",
            PaneKind::Changelog => "What's new (CHANGES): the changes to the terminal, newest first.",
            PaneKind::Wei => {
                "World equity indices (WEI): major indices by region with the day's change. Click one to chart it. \
                 A dot beside a ticker marks delayed, stale (amber) or sample (red) prices; hover for details."
            }
            PaneKind::Curve => "Yield curve (GC): US Treasury yields by maturity: today, a month ago and a year ago.",
            PaneKind::Search { .. } => {
                "Security search (SECF): find stocks, ETFs, indices, FX and crypto by name or ticker."
            }
            PaneKind::News(_) => "News (CN): headlines for the security, or top market news when none is loaded.",
            PaneKind::OrderBook(_) => {
                "Order book (OB): live bids and asks with depth, for markets with a public book (crypto)."
            }
            PaneKind::Options(_) => {
                "Option monitor (OMON): calls and puts by expiry and strike with prices and implied volatility."
            }
            PaneKind::Portfolio(_) => {
                "Portfolio (PORT): positions, orders, accounts and activity of the demo or connected account, \
                 with live P&L including pre- and after-market prices. Prices are the terminal's quotes (Yahoo is \
                 delayed about 15 minutes); the status bar warns when any shown price is sample data."
            }
            PaneKind::Settings => "Settings (PDF): appearance, panels, data keys, trading and layouts.",
            PaneKind::Edgar(e) if e.fa => {
                "Financials (FA): income statement, balance sheet, cash flow and ratios, annual or quarterly. US \
                 companies from SEC filings (XBRL); Borsa Istanbul companies (e.g. THYAO, AKBNK) from İş Yatırım, \
                 with a TRY / USD switch because TMS 29 inflation accounting distorts TRY comparisons, banks on \
                 their own template, and year-to-date periods (3M/6M/9M) as published instead of single quarters. Type a ticker in the box and press Enter; click a row to chart that line."
            }
            PaneKind::Edgar(_) => {
                "Description (DES): company profile, key figures and valuation at the live price. US companies \
                 from SEC EDGAR (business, sector, fiscal year, recent filings); Borsa Istanbul companies from \
                 İş Yatırım (address, capital, free float, shareholders). The price tag shows its source and delay. \
                 Type a ticker in the box and press Enter."
            }
            PaneKind::Earnings(e) if e.symbol.is_some() => {
                "Earnings history (TICKER ERN): past and next report dates with EPS estimate vs actual, the \
                 surprise and the stock's move on the report. Source: Nasdaq calendar and SEC filings."
            }
            PaneKind::Earnings(_) => {
                "Earnings calendar (ERN): which companies report each day of the week, before the open (BMO) \
                 or after the close (AMC), with EPS estimates. Your watchlist and positions are highlighted; \
                 click a row for that company's history."
            }
            PaneKind::Eco(_) => {
                "Economic calendar (ECO): this week's releases worldwide (US from FRED: CPI, payrolls, FOMC…; \
                 EUR, GBP, JPY, CNY and others from ForexFactory) with consensus, actual, prior and \
                 importance. Filter countries with the chips or ECO US EU GB JP CN, ECO ALL for all. Click a \
                 US row for its history (ECOF)."
            }
            PaneKind::Etf(_) => {
                "US ETFs (ETF): pick a fund for its largest holdings as a donut chart and list, plus sector weights. \
                 Type a ticker to open any ETF (ETF SPY). 'Held by' shows which indexed ETFs hold a stock; 'Overlap' \
                 compares two funds (ETF VOO QQQ); AAPL ETFS lists the funds holding a stock. Holdings come from SEC N-PORT filings (full portfolio, as of the \
                 filing's report date, usually 2-4 months old); Yahoo supplies the top 10 for funds without N-PORT \
                 (SPY, DIA, GLD) and the sector weights. Reverse search and overlap cover the funds indexed on disk: \
                 use 'Index curated list' to fetch the popular ones."
            }
            PaneKind::Most(_) => {
                "Market movers (MOST): US stocks with the biggest day gains, biggest day losses and highest volume, from \
                 Yahoo's predefined screeners (top 100 each). Click a symbol or name for its chart, right-click for more; \
                 type in the filter box to narrow by symbol or name. Rel Vol is today's volume over the 3-month average. \
                 Refreshes every minute while visible. Prices are delayed about 15 minutes; pre-market and after-hours \
                 moves are not included."
            }
            PaneKind::Fut(_) => {
                "Futures and volatility (FUT, VIX, HEAT). Futures Curve: the next ~12 contract months of a futures root (pick a chip: ES, CL, GC, ZN...) as a line chart and table with change and volume, plus contango / backwardation and the annualised roll yield between the front and last contract; contract months Yahoo does not list are skipped. VIX Term Structure: VIX9D, VIX, VIX3M, VIX6M and VIX1Y from Yahoo, plus the monthly VIX futures from Cboe's end-of-day settlement file (previous close; intraday VX quotes need a browser session and are not available), the VIX/VIX3M ratio and a short reading. Sector Heatmap: the 11 SPDR sector ETFs (or a world mini-map of country ETFs) coloured by 1D / 1W / 1M / YTD change; tile area uses approximate fixed weights, not live market caps. Click a tile for its chart, right-click for the ETF's holdings. Data is delayed; refreshes every minute while visible."
            }
            PaneKind::Smart(_) => {
                "Smart money (SMART): what insiders, big institutions and short sellers are doing in the active symbol. \
                 Insiders: recent Form 4 transactions (buys, sells, size, shares owned after) and a net buy/sell \
                 summary for the last 90 days. Institutions: the latest 13F-HR of ~18 well-known managers (top 25 \
                 positions, new/added/reduced/sold vs the previous quarter) and which of them hold the symbol; use \
                 'Index curated managers' to download them. Short interest: FINRA's twice-monthly short interest \
                 and days to cover, plus daily short SALE volume (a different measure, labelled as such). 13F data is \
                 6 weeks to 4½ months old. Sources: SEC EDGAR, FINRA. Follows the active symbol (Sync) or type a ticker."
            }
            PaneKind::Top(_) => {
                "Top news (TOP): market headlines from public RSS feeds, newest first. World (CNBC, MarketWatch), Türkiye \
                 (Bloomberg HT, AA), Crypto (CoinDesk, The Block) and Central Banks (Fed, ECB) tabs, or All merged and \
                 de-duplicated. Headlines naming a ticker in your watchlists are amber with a ticker chip that opens its \
                 chart. Click a headline to read it here (Back returns); pay-walled pages fall back to Open in Browser. \
                 Refreshes every 2 minutes while visible; a feed that fails is named and its last copy kept. TOP TR, \
                 TOP WORLD, TOP CRYPTO and TOP CB pick a tab."
            }
            PaneKind::TrFunds(_) => {
                "Turkish funds (TRF): every investment fund on KAP with a search box; click a code for the fund card \
                 (fees, size, manager), asset allocation and top-10 stock allocation as donut charts, and the buys and \
                 sells between the latest two monthly portfolio reports. 'Held by' (or THYAO TRFS) lists which indexed \
                 funds hold a BIST stock; 'Index curated list' downloads popular funds' reports to disk. Data: KAP \
                 'Portföy Dağılım Raporu' PDFs (published in the first days of each month) and fund information forms. \
                 Redemption valör comes from the fund's prospectus on KAP; purchase valör is not stated there (n/a)."
            }
            PaneKind::Audit(_) => {
                "Audit log (AUDIT): every order attempt (time, account, symbol, side, size, type, price, the pre-trade checks, \
                 whether you confirmed warnings, and the outcome) plus kill-switch and limit changes, newest first. Filter by \
                 symbol or outcome; Export CSV writes the rows shown to the data folder. The files are append-only JSONL, one \
                 per month, in the audit folder. The Holidays tab (HOL) lists NYSE and Borsa Istanbul closures and early closes."
            }
            PaneKind::Risk(_) => {
                "Portfolio risk (RISK, CORR): computed locally from about two years of daily history of your positions, \
                 weighted by current market value. Overview: annualised volatility, historical and parametric VaR \
                 (95/99%), expected shortfall, beta, alpha, R\u{b2}, tracking error and Sharpe/Sortino against a benchmark \
                 (SPY, QQQ, XU100 or RISK <ticker>; the risk-free rate is a fixed 4% assumption), max/current drawdown and \
                 an equity/drawdown chart. Contribution: each position's share of portfolio volatility. Correlation: \
                 heatmap of daily returns, click a cell to chart that symbol. Options, cash and instruments without \
                 price history are listed as not included with the reason; values are not currency-converted."
            }
        }
    }

    /// The security this pane follows, for symbol-driven panels.
    fn symbol(&self) -> Option<&str> {
        match self {
            PaneKind::Chart(c) => Some(&c.symbol),
            PaneKind::Ticker { symbol, .. } => Some(symbol),
            PaneKind::News(n) => n.symbol.as_deref(),
            PaneKind::OrderBook(b) => Some(&b.symbol),
            PaneKind::Options(o) => Some(&o.symbol),
            PaneKind::Edgar(e) => Some(&e.symbol),
            PaneKind::Smart(e) => Some(&e.symbol),
            PaneKind::Earnings(e) => e.symbol.as_deref(),
            _ => None,
        }
    }

    /// Placeholder of the pane's filter box, for panes whose rows can be filtered.
    fn search_hint(&self) -> Option<&'static str> {
        Some(match self {
            PaneKind::Watchlist | PaneKind::List(_) => "Filter by ticker…",
            PaneKind::Feed { .. } => "Filter by politician, ticker or asset…",
            PaneKind::Ticker { .. } => "Filter by politician or asset…",
            PaneKind::Profile { .. } => "Filter trades by ticker or asset…",
            PaneKind::Politicians => "Filter by name or state…",
            PaneKind::Alerts => "Filter by politician or message…",
            PaneKind::Wei => "Filter by ticker or name…",
            PaneKind::News(_) => "Filter headlines…",
            PaneKind::Portfolio(_) => "Filter by symbol…",
            PaneKind::Earnings(_) => "Filter by ticker or company…",
            PaneKind::Eco(_) => "Filter by event or code…",
            PaneKind::Etf(_) => "Find an ETF, or a holding by ticker or name…",
            PaneKind::TrFunds(_) => "Find a fund, or a stock in it…",
            _ => return None,
        })
    }

    fn follows_symbol(&self) -> bool {
        matches!(
            self,
            PaneKind::Chart(_)
                | PaneKind::Ticker { .. }
                | PaneKind::News(_)
                | PaneKind::OrderBook(_)
                | PaneKind::Options(_)
                | PaneKind::Edgar(_)
                | PaneKind::Smart(_)
        ) || matches!(self, PaneKind::Earnings(e) if e.symbol.is_some())
    }

    fn settings(&self) -> PaneSettings {
        match self {
            PaneKind::Chart(c) => PaneSettings::Chart {
                symbol: c.symbol.clone(),
                tf: c.tf,
                chart_type: c.chart_type,
                log: c.log,
                indicators: c.indicators.clone(),
                studies: Some(c.studies.clone()),
                drawings: c.drawings.clone(),
                compare: c.compare.clone(),
                show_volume: c.show_volume,
                show_congress: c.show_congress,
                magnet: c.magnet,
            },
            PaneKind::Feed { filter } => {
                PaneSettings::Feed((*filter != FeedFilter::default()).then(|| (*filter).into()))
            }
            PaneKind::Ticker { symbol, .. } => PaneSettings::Ticker(symbol.clone()),
            PaneKind::Profile { query, .. } => PaneSettings::Profile(query.clone()),
            PaneKind::Politicians => PaneSettings::Politicians,
            PaneKind::Alerts => PaneSettings::Alerts,
            PaneKind::Watchlist => PaneSettings::Watchlist,
            PaneKind::List(name) => PaneSettings::List(name.clone()),
            PaneKind::Help => PaneSettings::Help,
            PaneKind::Changelog => PaneSettings::Changelog,
            PaneKind::Wei => PaneSettings::Wei,
            PaneKind::Curve => PaneSettings::Curve,
            PaneKind::Search { query } => PaneSettings::Search(query.clone()),
            PaneKind::News(n) => PaneSettings::News(n.symbol.clone()),
            PaneKind::OrderBook(b) => {
                PaneSettings::OrderBook { symbol: b.symbol.clone(), depth: b.depth, group: b.group }
            }
            PaneKind::Settings => PaneSettings::Settings,
            PaneKind::Options(o) => {
                PaneSettings::Options { symbol: o.symbol.clone(), strikes: o.strikes, greeks: o.show_greeks }
            }
            PaneKind::Earnings(e) => PaneSettings::Earnings { symbol: e.symbol.clone(), scope: e.scope, cap: e.cap },
            PaneKind::Portfolio(p) => PaneSettings::Portfolio {
                view: p.view,
                account: p.account.clone(),
                symbol: p.symbol.clone(),
                sort: p.sort,
                open_only: p.open_only,
            },
            PaneKind::Edgar(e) => e.settings(),
            PaneKind::Eco(e) => PaneSettings::Eco {
                min_importance: e.min_importance,
                series: e.series_code.clone(),
                countries: e.countries.iter().cloned().collect(),
            },
            PaneKind::Etf(e) => {
                let (mode, a, b) = e.settings();
                PaneSettings::Etf { mode: mode.into(), a, b }
            }
            PaneKind::TrFunds(e) => {
                let (mode, a) = e.settings();
                PaneSettings::TrFunds { mode: mode.into(), a }
            }
            PaneKind::Most(e) => PaneSettings::Most { list: e.settings().into() },
            PaneKind::Top(e) => PaneSettings::Top { tab: e.settings().into() },
            PaneKind::Fut(e) => {
                let (tab, root, period, world) = e.settings();
                PaneSettings::Fut { tab: tab.into(), root, period: period.into(), world }
            }
            PaneKind::Smart(e) => e.settings(),
            PaneKind::Risk(e) => {
                let (tab, benchmark, window) = e.settings();
                PaneSettings::Risk { tab: tab.into(), benchmark, window }
            }
            PaneKind::Audit(e) => PaneSettings::Audit { tab: e.settings().into() },
        }
    }

    fn from_settings(p: PaneSettings) -> PaneKind {
        match p {
            PaneSettings::Chart {
                symbol,
                tf,
                chart_type,
                log,
                indicators,
                studies,
                drawings,
                compare,
                show_volume,
                show_congress,
                magnet,
            } => {
                let mut c = ChartState::new(0, &symbol, tf);
                c.chart_type = chart_type;
                c.log = log;
                c.indicators = indicators;
                if let Some(s) = studies {
                    c.studies = s;
                }
                c.drawings = drawings;
                c.compare = compare;
                c.show_volume = show_volume;
                c.show_congress = show_congress;
                c.magnet = magnet;
                PaneKind::Chart(Box::new(c))
            }
            PaneSettings::Feed(f) => PaneKind::Feed { filter: f.map(Into::into).unwrap_or_default() },
            PaneSettings::Ticker(s) => PaneKind::Ticker { symbol: s, trades: Vec::new(), loading: true },
            PaneSettings::Profile(q) => PaneKind::Profile { query: q, data: None, loading: true },
            PaneSettings::Politicians => PaneKind::Politicians,
            PaneSettings::Alerts => PaneKind::Alerts,
            PaneSettings::Watchlist => PaneKind::Watchlist,
            PaneSettings::List(name) => PaneKind::List(name),
            PaneSettings::Help => PaneKind::Help,
            PaneSettings::Changelog => PaneKind::Changelog,
            PaneSettings::Wei => PaneKind::Wei,
            PaneSettings::Curve => PaneKind::Curve,
            PaneSettings::Search(q) => PaneKind::Search { query: q },
            PaneSettings::News(s) => PaneKind::News(Box::new(NewsPane::new(s))),
            PaneSettings::OrderBook { symbol, depth, group } => {
                PaneKind::OrderBook(Box::new(BookPane::new(&symbol, depth, group)))
            }
            PaneSettings::Settings => PaneKind::Settings,
            PaneSettings::Options { symbol, strikes, greeks } => {
                PaneKind::Options(Box::new(OptionsPane::new(&symbol, strikes, greeks)))
            }
            PaneSettings::Earnings { symbol, scope, cap } => PaneKind::Earnings(Box::new(match symbol {
                Some(s) => ErnPane::ticker(&s),
                None => ErnPane::calendar(scope, cap),
            })),
            PaneSettings::Portfolio { view, account, symbol, sort, open_only } => {
                PaneKind::Portfolio(Box::new(broker::PortfolioPane {
                    view,
                    account,
                    symbol,
                    sort,
                    open_only,
                    query: String::new(),
                }))
            }
            PaneSettings::Edgar { symbol, fa, quarterly, tab, usd } => {
                PaneKind::Edgar(Box::new(edgar_view::EdgarPane::from_settings(&symbol, fa, quarterly, tab, usd)))
            }
            PaneSettings::Etf { mode, a, b } => PaneKind::Etf(Box::new(etf_view::EtfPane::new(&mode, &a, &b))),
            PaneSettings::Risk { tab, benchmark, window } => {
                PaneKind::Risk(Box::new(risk_view::RiskPane::new(&tab, &benchmark, window)))
            }
            PaneSettings::Audit { tab } => PaneKind::Audit(Box::new(safety_view::AuditPane::new(&tab))),
            PaneSettings::TrFunds { mode, a } => PaneKind::TrFunds(Box::new(trfunds_view::TrfPane::new(&mode, &a))),
            PaneSettings::Most { list } => PaneKind::Most(Box::new(most_view::MostPane::new(&list))),
            PaneSettings::Top { tab } => PaneKind::Top(Box::new(top_view::TopPane::new(&tab))),
            PaneSettings::Fut { tab, root, period, world } => {
                PaneKind::Fut(Box::new(fut_view::FutPane::from_settings(&tab, &root, &period, world)))
            }
            PaneSettings::Smart { symbol, tab } => {
                PaneKind::Smart(Box::new(smart_view::SmartPane::from_settings(&symbol, &tab)))
            }
            PaneSettings::Eco { min_importance, series, countries } => {
                PaneKind::Eco(Box::new(eco_view::EcoPane::new(min_importance, series).with_countries(&countries)))
            }
        }
    }
}

pub struct Pane {
    pub id: PaneId,
    pub kind: PaneKind,
    pub link: Option<Link>,
    /// User-chosen tab title.
    pub title: Option<String>,
    /// Set while popped out into its own window.
    pub window: Option<WindowGeometry>,
}

impl Pane {
    fn display_title(&self) -> String {
        self.title.clone().unwrap_or_else(|| self.kind.tab_title())
    }
}

/// App-wide state persisted in `settings.json` (layouts live in their own files).
#[derive(Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    watchlist: Vec<String>,
    #[serde(default)]
    wei_order: Vec<String>,
    #[serde(default)]
    groups: Option<Vec<WatchGroup>>,
    /// Extra watchlist columns and sorting, per list (the main watchlist is the "" key).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    wl_columns: BTreeMap<String, crate::wlcols::ListCols>,
    #[serde(default = "default_true")]
    sidebar: bool,
    /// The sidebar's watchlist section is unfolded.
    #[serde(default = "default_true")]
    watchlist_open: bool,
    /// Name of the layout to restore on launch.
    #[serde(default)]
    active_layout: Option<String>,
    // Pre-docking settings files stored a preset and a pane list here.
    #[serde(default, skip_serializing)]
    layout: Option<Layout>,
    #[serde(default, skip_serializing)]
    panes: Option<Vec<serde_json::Value>>,
}

fn default_true() -> bool {
    true
}

/// BIST 30 constituents, most traded first (average daily TRY turnover over the 60 sessions
/// before 15 Oct 2026).
const BIST30_BY_VOLUME: [&str; 30] = [
    "THYAO", "ASELS", "AKBNK", "TUPRS", "ASTOR", "YKBNK", "ISCTR", "KCHOL", "EREGL", "GARAN", "BIMAS", "SAHOL",
    "EKGYO", "TCELL", "SISE", "KRDMD", "TTKOM", "PETKM", "HALKB", "VAKBN", "MGROS", "SASA", "FROTO", "PGSUS", "BRSAN",
    "GUBRF", "ENKAI", "TOASO", "TAVHL", "ALARK",
];

/// A named, collapsible list in the sidebar, beside the main watchlist.
#[derive(Clone, Serialize, Deserialize)]
struct WatchGroup {
    name: String,
    keys: Vec<String>,
    open: bool,
}

impl WatchGroup {
    fn bist30() -> Self {
        let keys = BIST30_BY_VOLUME.iter().map(|t| tr_data::security(t).key()).collect();
        Self { name: "BIST 30 · by volume".into(), keys, open: true }
    }
}

const DEFAULT_WATCHLIST: [&str; 11] = [
    "SPX Index",
    "AAPL US Equity",
    "NVDA US Equity",
    "SPY US Equity",
    "BTCUSD Curncy",
    "ETHUSD Curncy",
    "EURUSD Curncy",
    "USDJPY Curncy",
    "US10Y Govt",
    "CL1 Comdty",
    "GC1 Comdty",
];

/// Convert a pre-docking settings file into a layout document.
fn migrate_legacy(s: &Settings, defaults: &prefs::PanelDefaults) -> Option<LayoutDoc> {
    let panes: Vec<PaneSettings> = s
        .panes
        .as_ref()?
        .iter()
        .filter_map(|v| {
            // Unit variants were stored as bare strings ("Feed"); wrap them as {"Feed": null}.
            let v = match v {
                serde_json::Value::String(name) => serde_json::json!({ name.as_str(): null }),
                other => other.clone(),
            };
            serde_json::from_value(v).ok()
        })
        .collect();
    if panes.is_empty() {
        return None;
    }
    let _ = defaults;
    Some(LayoutDoc::preset("Default", s.layout.unwrap_or(Layout::Grid2x2), panes))
}

/// A pane drag from its tab or header: pending until the pointer moves past a small
/// threshold (so clicks still select tabs), then active until release.
#[derive(Debug, Clone, Copy)]
pub struct Drag {
    pub pane: PaneId,
    pub origin: egui::Pos2,
    pub active: bool,
}

pub struct TerminalApp {
    worker: Worker,
    provider: String,
    panes: Vec<Pane>,
    dock: Node,
    focus: PaneId,
    /// The chart pane that was focused most recently (target of the pane Sync buttons).
    last_chart: Option<PaneId>,
    next_id: PaneId,
    last_opened: Option<PaneId>,
    /// Preset last applied, for the toolbar picker (cleared by manual rearranging).
    preset: Option<Layout>,
    drag: Option<Drag>,
    /// Tab being renamed inline: pane and edit buffer.
    renaming_tab: Option<(PaneId, String)>,

    store: Store,
    layout_name: String,
    layout_names: Vec<String>,
    last_saved: Vec<u8>,
    /// Layout being renamed in the sidebar: old name and edit buffer.
    renaming_layout: Option<(String, String)>,

    prefs: Prefs,
    /// Built-in and custom themes, and the theme last applied to egui.
    themes: Vec<crate::themes::Theme>,
    theme_store: crate::themes::ThemeStore,
    applied_theme: Option<crate::themes::Theme>,
    /// Theme selected in the editor.
    editing_theme: String,
    /// Custom themes edited but not yet written (saved once editing pauses).
    dirty_themes: Vec<String>,
    themes_changed_at: Option<Instant>,
    renaming_theme: Option<(String, String)>,
    theme_io: String,
    /// The OS appearance, for System theme resolution in commands.
    system_theme: Option<egui::Theme>,
    /// The active layout carries its theme.
    layout_theme: bool,
    rebinding: Option<prefs::Action>,
    hotkey_filter: String,
    settings_tab: SettingsTab,
    io_path: String,

    curves: Option<Box<YieldCurves>>,
    last_wei: Option<Instant>,
    last_curve: Option<Instant>,
    wei_updated: Option<String>,
    /// Drag-arranged World Markets order (market keys), applied within each group.
    wei_order: Vec<String>,
    /// Extra sidebar lists under the watchlist.
    groups: Vec<WatchGroup>,
    /// The sidebar's watchlist section is unfolded (the extra lists keep their own flag).
    watchlist_open: bool,
    /// Watchlist column settings and their statistics.
    wl: crate::wlcols::State,
    /// Latest network search results and the query they answer.
    net_hits: (String, Vec<SearchHit>),
    /// Query typed in the command line awaiting a debounced network search.
    search_pending: Option<(String, Instant)>,
    suggestion: Option<usize>,
    /// The pointer is over the autocomplete list (it must outlive the field losing focus to a click).
    dropdown_hover: bool,

    cmd: String,
    cmd_history: Vec<String>,
    cmd_hist_pos: Option<usize>,
    focus_cmd: bool,

    status: String,
    toasts: Vec<(String, Instant)>,

    latest: Vec<Trade>,
    politicians: Vec<PoliticianSummary>,
    /// Open in-pane filter boxes, keyed by pane.
    pane_search: HashMap<PaneId, String>,
    /// Pane whose filter box takes the keyboard on its next draw.
    search_focus: Option<PaneId>,
    alerts: Vec<Alert>,
    /// Price/indicator alert rules (the worker owns and evaluates them; this is its latest copy).
    price_alerts: crate::alerts::AlertBook,
    congress_status: String,
    /// When the last Congress ingest finished.
    congress_checked: Option<chrono::DateTime<chrono::Local>>,
    /// Earnings toasts already shown and the last watchlist check.
    earn: earnings_view::EarningsState,

    watchlist: Vec<String>,
    quotes: HashMap<String, Quote>,
    last_quotes: Option<Instant>,
    last_feed: Option<Instant>,
    last_save: Instant,
    started: Instant,
    smoke_done: bool,
    /// Frame-time readout (F12): smoothed CPU time of `ui()` and interval between frames.
    show_perf: bool,
    perf_cpu_ms: f64,
    perf_dt_ms: f64,
    last_frame: Option<Instant>,

    sidebar_open: bool,
    /// System accent colour (macOS) or the default blue.
    system_accent: Color32,
    last_accent_poll: Option<Instant>,
    window_styled: bool,
    /// The window is see-through behind the sidebar (macOS vibrancy).
    vibrancy: bool,
    /// Measured macOS title bar geometry (polled; changes with full screen).
    chrome: Option<tr_macos::Chrome>,
    last_chrome_poll: Option<Instant>,
    /// Brokerage integration (SnapTrade credentials and the loaded portfolio).
    broker: broker::BrokerState,
    /// Order ticket and trading dialogs.
    trading: trading::TradingState,
    /// The paper-trading demo account (selected by default).
    demo: demo::DemoState,
    /// Pre-trade limits, kill switch and the audit log.
    safety: safety_view::SafetyState,
    /// ⌘K command palette.
    palette: crate::palette::Palette,
}

/// Resolve a run of words (`BTCUSD CRNCY AAPL US EQUITY MSFT`) into market keys.
fn keys_from_words(words: &[String]) -> Vec<String> {
    let tokens: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match tr_core::Security::resolve_tokens(&tokens[i..]) {
            Some((sec, used)) => {
                out.push(sec.key());
                i += used;
            }
            None => i += 1,
        }
    }
    out
}

fn settings_path() -> std::path::PathBuf {
    data_dir().join("settings.json")
}

fn prefs_path() -> std::path::PathBuf {
    data_dir().join("prefs.json")
}

impl TerminalApp {
    pub fn new(ctx: &egui::Context, worker: Worker, provider: String) -> Self {
        let system_accent =
            tr_macos::accent_color().map_or(theme::DEFAULT_ACCENT, |[r, g, b]| Color32::from_rgb(r, g, b));
        let mut prefs = Prefs::load(&prefs_path());
        let theme_store = crate::themes::ThemeStore::new(data_dir().join("themes"));
        let mut themes = theme_store.load_all();
        // Appearance overrides from older versions become a custom theme file.
        if let Some(mut t) = prefs.migrated_theme.take() {
            t.name = crate::themes::unique_name(&themes, &t.name);
            prefs.appearance.theme = t.name.clone();
            match theme_store.save(&t) {
                Ok(path) => t.source = Some(path),
                Err(e) => tracing::warn!("saving migrated theme: {e:#}"),
            }
            themes.push(t);
            let _ = prefs.save(&prefs_path());
        }
        if let Ok(v) = std::env::var("TR_THEME") {
            prefs.appearance.theme = match v.to_ascii_lowercase().as_str() {
                "light" => prefs.appearance.light_theme.clone(),
                "dark" => prefs.appearance.dark_theme.clone(),
                _ => crate::themes::find(&themes, &v).map_or(prefs.appearance.theme.clone(), |t| t.name.clone()),
            };
        }
        ctx.set_zoom_factor(prefs.appearance.zoom_factor());
        let settings: Option<Settings> =
            std::fs::read(settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let broker = broker::BrokerState::load(provider.eq_ignore_ascii_case("sample"));
        let (demo, demo_note) = demo::DemoState::load(demo::demo_path());
        let mut app = Self {
            broker,
            trading: Default::default(),
            demo,
            safety: safety_view::SafetyState::load(),
            worker,
            provider,
            panes: Vec::new(),
            dock: Node::default(),
            focus: 0,
            last_chart: None,
            next_id: 1,
            last_opened: None,
            preset: None,
            drag: None,
            renaming_tab: None,
            store: Store::new(data_dir().join("layouts")),
            layout_name: "Default".into(),
            layout_names: Vec::new(),
            last_saved: Vec::new(),
            renaming_layout: None,
            editing_theme: prefs.appearance.theme.clone(),
            themes,
            theme_store,
            applied_theme: None,
            dirty_themes: Vec::new(),
            themes_changed_at: None,
            renaming_theme: None,
            theme_io: String::new(),
            system_theme: ctx.system_theme(),
            layout_theme: false,
            prefs,
            rebinding: None,
            hotkey_filter: String::new(),
            settings_tab: SettingsTab::Appearance,
            io_path: String::new(),
            curves: None,
            last_wei: None,
            last_curve: None,
            wei_updated: None,
            wei_order: Vec::new(),
            groups: vec![WatchGroup::bist30()],
            watchlist_open: true,
            wl: Default::default(),
            net_hits: (String::new(), Vec::new()),
            search_pending: None,
            suggestion: None,
            dropdown_hover: false,
            cmd: String::new(),
            cmd_history: Vec::new(),
            cmd_hist_pos: None,
            focus_cmd: true,
            status: "Ready. Type a ticker and press <GO> (Enter). F1 for help.".into(),
            toasts: Vec::new(),
            latest: Vec::new(),
            politicians: Vec::new(),
            pane_search: HashMap::new(),
            search_focus: None,
            alerts: Vec::new(),
            price_alerts: Default::default(),
            congress_status: String::new(),
            congress_checked: None,
            earn: Default::default(),
            watchlist: DEFAULT_WATCHLIST.map(String::from).to_vec(),
            quotes: HashMap::new(),
            last_quotes: None,
            last_feed: None,
            last_save: Instant::now(),
            started: Instant::now(),
            smoke_done: false,
            show_perf: std::env::var_os("TR_FPS").is_some(),
            perf_cpu_ms: 0.0,
            perf_dt_ms: 0.0,
            last_frame: None,
            sidebar_open: true,
            system_accent,
            last_accent_poll: None,
            window_styled: false,
            vibrancy: false,
            chrome: None,
            last_chrome_poll: None,
            palette: crate::palette::Palette::load(&data_dir().join("palette.json")),
        };
        let mut doc = None;
        if let Some(s) = &settings {
            if !s.watchlist.is_empty() {
                // Older settings stored bare tickers; normalise to market keys.
                app.watchlist = s.watchlist.iter().map(|w| tr_data::security(w).key()).collect();
                app.watchlist.dedup();
            }
            app.wei_order = s.wei_order.clone();
            app.wl.lists = s.wl_columns.clone();
            if let Some(g) = &s.groups {
                app.groups = g.clone();
            }
            app.sidebar_open = s.sidebar;
            app.watchlist_open = s.watchlist_open;
            if let Some(name) = &s.active_layout {
                match app.store.load(name) {
                    Ok(d) => doc = Some(d),
                    Err(e) => tracing::warn!("restoring layout {name:?}: {e:#}"),
                }
            }
            if doc.is_none() {
                doc = migrate_legacy(s, &app.prefs.panels);
            }
        }
        let doc = doc
            .or_else(|| app.store.list().first().and_then(|n| app.store.load(n).ok()))
            .unwrap_or_else(|| LayoutDoc::default_workspace("Default", &app.prefs.panels));
        app.apply_doc(doc);
        if !app.store.exists(&app.layout_name) {
            app.save_layout();
        }
        app.refresh_layout_names();
        app.worker.send(Request::Latest { limit: 2_000 });
        app.worker.send(Request::SearchPoliticians(String::new()));
        app.worker.send(Request::Alerts);
        if let Some(note) = demo_note {
            app.trade_toast(note, false);
        }
        app
    }

    fn save_settings(&self) {
        let s = Settings {
            watchlist: self.watchlist.clone(),
            wei_order: self.wei_order.clone(),
            groups: Some(self.groups.clone()),
            wl_columns: self.wl.lists.clone(),
            sidebar: self.sidebar_open,
            watchlist_open: self.watchlist_open,
            active_layout: Some(self.layout_name.clone()),
            layout: None,
            panes: None,
        };
        if let Ok(json) = serde_json::to_vec_pretty(&s)
            && let Err(e) = crate::layouts::write_atomic(&settings_path(), &json)
        {
            tracing::warn!("saving settings: {e}");
        }
    }

    fn save_prefs(&mut self) {
        if let Err(e) = self.prefs.save(&prefs_path()) {
            self.status = format!("Could not save preferences: {e}");
        }
    }
}

/// Apply the native macOS window chrome (unified toolbar, inset traffic lights, sidebar
/// vibrancy). Returns whether the window is now translucent.
#[cfg(target_os = "macos")]
fn style_native_window(frame: &eframe::Frame, vibrancy: bool) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let vibrancy = vibrancy && std::env::var("TR_VIBRANCY").map_or(true, |v| v != "0");
    let Ok(handle) = frame.window_handle() else { return false };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return false };
    // SAFETY: called on the main thread (eframe runs `ui` there) with a live NSView.
    let ok = unsafe { tr_macos::style_window(h.ns_view.as_ptr(), vibrancy) };
    tracing::info!("macOS window chrome applied: {ok} (vibrancy {vibrancy})");
    ok && vibrancy
}

#[cfg(not(target_os = "macos"))]
fn style_native_window(_frame: &eframe::Frame, _vibrancy: bool) -> bool {
    false
}

/// The main window's live title bar geometry (macOS).
#[cfg(target_os = "macos")]
fn native_chrome(frame: &eframe::Frame) -> Option<tr_macos::Chrome> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = frame.window_handle().ok()?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return None };
    // SAFETY: main thread, live NSView owned by the window.
    unsafe { tr_macos::chrome_metrics(h.ns_view.as_ptr()) }
}

#[cfg(not(target_os = "macos"))]
fn native_chrome(_frame: &eframe::Frame) -> Option<tr_macos::Chrome> {
    None
}

impl eframe::App for TerminalApp {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.vibrancy { [0.0; 4] } else { visuals.window_fill.to_normalized_gamma_f32() }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        if let Some(prev) = self.last_frame.replace(frame_start) {
            let dt = prev.elapsed().as_secs_f64() * 1e3;
            self.perf_dt_ms = if self.perf_dt_ms == 0.0 { dt } else { self.perf_dt_ms * 0.9 + dt * 0.1 };
        }
        let ctx = ui.ctx().clone();
        if !self.window_styled {
            self.window_styled = true;
            self.vibrancy = style_native_window(frame, self.prefs.appearance.vibrancy);
        }
        if self.last_chrome_poll.is_none_or(|t| t.elapsed() > Duration::from_millis(750)) {
            let c = native_chrome(frame);
            if c != self.chrome {
                tracing::info!("window chrome: {c:?}");
                self.chrome = c;
            }
            self.last_chrome_poll = Some(Instant::now());
        }
        if self.last_accent_poll.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            if let Some([r, g, b]) = tr_macos::accent_color() {
                self.system_accent = Color32::from_rgb(r, g, b);
            }
            self.last_accent_poll = Some(Instant::now());
        }
        self.system_theme = ctx.system_theme();
        let ti = self.active_theme_index(&ctx);
        if self.applied_theme.as_ref() != Some(&self.themes[ti]) {
            theme::configure(&ctx, &self.themes[ti], self.system_accent);
            self.applied_theme = Some(self.themes[ti].clone());
        }
        theme::refresh(&ctx, &self.themes[ti], self.system_accent);
        self.drain();
        self.periodic();

        let screen = ui.max_rect();
        let open = anim::spring_with(
            &ctx,
            egui::Id::new("sidebar-open"),
            if self.sidebar_open { 1.0 } else { 0.0 },
            0.32,
            0.9,
        );
        let sb_w = (SIDEBAR_W * open).round();
        let right_side = self.prefs.appearance.sidebar_side == prefs::SidebarSide::Right;
        let main = if right_side {
            Rect::from_min_max(screen.min, pos2(screen.right() - sb_w, screen.bottom()))
        } else {
            Rect::from_min_max(pos2(screen.left() + sb_w, screen.top()), screen.max)
        };
        ui.painter().rect_filled(main, 0.0, theme::window());
        let toolbar = Rect::from_min_size(main.min, vec2(main.width(), self.toolbar_h()));
        let status = Rect::from_min_max(pos2(main.left(), main.bottom() - STATUS_H), main.max);
        let gap = theme::dp(6.0);
        let area = Rect::from_min_max(
            pos2(main.left() + gap, toolbar.bottom() + gap),
            pos2(main.right() - gap, status.top() - gap * 0.5),
        );

        self.hotkeys(&ctx);
        self.command_palette(&ctx);
        let mut sidebar_cmds = Vec::new();
        if sb_w >= 1.0 {
            // The sidebar slides in from its edge.
            let x = if right_side { screen.right() - sb_w } else { screen.left() + sb_w - SIDEBAR_W };
            let full = Rect::from_min_size(pos2(x, screen.top()), vec2(SIDEBAR_W, screen.height()));
            let mut child = ui.new_child(UiBuilder::new().max_rect(full).id_salt("sidebar-root"));
            let visible = if right_side {
                Rect::from_min_max(pos2(screen.right() - sb_w, screen.top()), screen.max)
            } else {
                Rect::from_min_max(screen.min, pos2(screen.left() + sb_w, screen.bottom()))
            };
            child.set_clip_rect(visible);
            sidebar_cmds = self.sidebar(&mut child, full);
        }
        // The traffic lights sit at the window's left edge: clear them unless a left
        // sidebar already covers that corner.
        let inset = if right_side { self.traffic_inset() } else { (self.traffic_inset() - sb_w).max(0.0) };
        self.toolbar(ui, toolbar, inset);
        self.status_bar(ui, status);

        let mut actions = Vec::new();
        for c in sidebar_cmds {
            match c {
                SidebarCmd::Action(a) => actions.push(a),
                SidebarCmd::Function(f) => self.apply(f),
                SidebarCmd::Layout(op) => self.layout_op(op),
            }
        }
        for p in &mut self.panes {
            if let PaneKind::Chart(c) = &mut p.kind {
                c.watched = self.watchlist.contains(&c.symbol);
                c.alert_lines = self.price_alerts.chart_alerts(&c.symbol);
            }
        }
        actions.extend(self.render_dock(ui, area, gap));
        actions.extend(self.render_windows(&ctx));
        self.run_actions(actions);
        self.trading_windows(&ctx);
        self.toasts(&ctx);

        // TR_SMOKE_SECS=N: render for N seconds, log frame stats and quit (CI smoke test).
        if let Some(secs) = std::env::var("TR_SMOKE_SECS").ok().and_then(|v| v.parse::<f64>().ok()) {
            ctx.request_repaint();
            if !self.smoke_done && self.started.elapsed().as_secs_f64() > secs {
                self.smoke_done = true;
                tracing::info!(
                    "smoke: {} frames in {:.1}s, ui cpu {:.2} ms/frame, panes {}, layout {:?}",
                    ctx.cumulative_frame_nr(),
                    self.started.elapsed().as_secs_f64(),
                    self.perf_cpu_ms,
                    self.panes.len(),
                    self.layout_name
                );
                self.autosave();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        // egui's ⌘0 resets to 1.0; a ⌘0 no hotkey claimed returns to the default zoom instead.
        if ctx.input_mut(|i| i.consume_shortcut(&egui::gui_zoom::kb_shortcuts::ZOOM_RESET)) {
            ctx.set_zoom_factor(self.prefs.appearance.zoom_factor());
        }
        let cpu = frame_start.elapsed().as_secs_f64() * 1e3;
        self.perf_cpu_ms = self.perf_cpu_ms * 0.9 + cpu * 0.1;
        if self.show_perf {
            // Measure continuously while the readout is visible.
            ctx.request_repaint();
        }
        // Keep the clock ticking and pick up worker results without spinning.
        ctx.request_repaint_after(Duration::from_millis(500));
        if self.started.elapsed() < Duration::from_secs(2) {
            ctx.request_repaint();
        }
    }

    fn on_exit(&mut self) {
        self.autosave();
        if self.demo.save_error.is_some() {
            self.demo.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_settings_migrate_to_a_layout() {
        let old = r#"{"layout":"MainRight2","watchlist":["AAPL","BTCUSD Curncy"],"sidebar":false,
            "panes":[{"Chart":{"symbol":"NVDA","tf":"D1","chart_type":"Candles","log":false,"indicators":[],
            "drawings":[],"compare":null}},"Feed","Wei"]}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert!(!s.sidebar);
        let doc = migrate_legacy(&s, &prefs::PanelDefaults::default()).unwrap();
        assert_eq!(doc.panes.len(), 3);
        assert_eq!(doc.dock.visible(), vec![1, 2, 3]);
        assert!(matches!(&doc.panes[0].settings, PaneSettings::Chart { symbol, .. } if symbol == "NVDA"));
        assert_eq!(doc.panes[1].settings, PaneSettings::Feed(None));
        // New-format files carry no legacy fields and are not migrated.
        let new: Settings = serde_json::from_str(r#"{"watchlist":[],"active_layout":"Desk"}"#).unwrap();
        assert!(migrate_legacy(&new, &prefs::PanelDefaults::default()).is_none());
        assert_eq!(new.active_layout.as_deref(), Some("Desk"));
    }

    #[test]
    fn pane_settings_round_trip_through_kinds() {
        let d = prefs::PanelDefaults::default();
        for s in [
            crate::layouts::chart_settings("BTCUSD Curncy", &d),
            PaneSettings::Feed(Some(crate::layouts::FeedSettings { buys: Some(true), ..Default::default() })),
            PaneSettings::News(Some("AAPL US Equity".into())),
            PaneSettings::News(None),
            PaneSettings::OrderBook { symbol: "ETHUSD Curncy".into(), depth: 50, group: 1.0 },
            PaneSettings::Search("gold".into()),
            PaneSettings::Settings,
            PaneSettings::Wei,
        ] {
            assert_eq!(PaneKind::from_settings(s.clone()).settings(), s);
        }
    }
}
