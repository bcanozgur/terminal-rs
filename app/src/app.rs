//! Top-level terminal: command line, docked panes, saved layouts, hotkeys and status bar.
//!
//! Submodules: `chrome` (toolbar, sidebar, status bar), `workspace` (dock rendering,
//! drag and drop, pop-out windows, link groups, saved layouts), `panels` (per-kind pane
//! content) and `settings_view` (preferences UI).

mod broker;
mod changelog_view;
mod chrome;
mod demo;
mod des_view;
mod earnings;
mod eco_view;
mod edgar_view;
mod etf_view;
mod fa_view;
mod hotkeys_view;
mod most_view;
mod options_view;
mod palette_view;
mod panels;
mod portfolio_view;
mod settings_view;
mod smart_view;
mod theming;
mod trading;
mod trfunds_view;
mod workspace;

pub use broker::PosSort;

use std::collections::HashMap;
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
    /// Smart money (`SMART`): insiders (Form 4), institutions (13F) and short interest.
    Smart(Box<smart_view::SmartPane>),
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
        }
    }

    /// Store a chain and derive greeks from each contract's implied volatility.
    pub fn set_chain(&mut self, chain: Box<tr_data::OptionChain>, rate: f64, now: i64) {
        // Expiry at the 16:00 New York close (≈ 20:00 UTC) of the expiration date.
        let t = ((chain.expiration + 20 * 3600 - now) as f64 / (365.0 * 86_400.0)).max(0.5 / 365.0);
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

pub struct NewsPane {
    /// `None` = general market headlines.
    pub symbol: Option<String>,
    pub items: Vec<NewsItem>,
    pub error: Option<String>,
    pub loading: bool,
    pub fetched: Option<Instant>,
}

impl NewsPane {
    pub fn new(symbol: Option<String>) -> Self {
        Self { symbol, items: Vec::new(), error: None, loading: true, fetched: None }
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
            PaneKind::Smart(e) => e.title(),
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
            PaneKind::Smart(e) => e.code(),
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
                "Economic calendar (ECO): this week's US releases (CPI, payrolls, retail sales, FOMC…) with \
                 consensus, actual, prior and change. Actual vs consensus is the surprise markets react to. \
                 Click a row for its history (ECOF). Sources: FRED, ForexFactory."
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
            PaneKind::Smart(_) => {
                "Smart money (SMART): what insiders, big institutions and short sellers are doing in the active symbol. \
                 Insiders: recent Form 4 transactions (buys, sells, size, shares owned after) and a net buy/sell \
                 summary for the last 90 days. Institutions: the latest 13F-HR of ~18 well-known managers (top 25 \
                 positions, new/added/reduced/sold vs the previous quarter) and which of them hold the symbol; use \
                 'Index curated managers' to download them. Short interest: FINRA's twice-monthly short interest \
                 and days to cover, plus daily short SALE volume (a different measure, labelled as such). 13F data is \
                 6 weeks to 4½ months old. Sources: SEC EDGAR, FINRA. Follows the active symbol (Sync) or type a ticker."
            }
            PaneKind::TrFunds(_) => {
                "Turkish funds (TRF): every investment fund on KAP with a search box; click a code for the fund card \
                 (fees, size, manager), asset allocation and top-10 stock allocation as donut charts, and the buys and \
                 sells between the latest two monthly portfolio reports. 'Held by' (or THYAO TRFS) lists which indexed \
                 funds hold a BIST stock; 'Index curated list' downloads popular funds' reports to disk. Data: KAP \
                 'Portföy Dağılım Raporu' PDFs (published in the first days of each month) and fund information forms. \
                 Redemption valör comes from the fund's prospectus on KAP; purchase valör is not stated there (n/a)."
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
            PaneKind::Eco(e) => PaneSettings::Eco { min_importance: e.min_importance, series: e.series_code.clone() },
            PaneKind::Etf(e) => {
                let (mode, a, b) = e.settings();
                PaneSettings::Etf { mode: mode.into(), a, b }
            }
            PaneKind::TrFunds(e) => {
                let (mode, a) = e.settings();
                PaneSettings::TrFunds { mode: mode.into(), a }
            }
            PaneKind::Most(e) => PaneSettings::Most { list: e.settings().into() },
            PaneKind::Smart(e) => e.settings(),
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
            PaneSettings::TrFunds { mode, a } => PaneKind::TrFunds(Box::new(trfunds_view::TrfPane::new(&mode, &a))),
            PaneSettings::Most { list } => PaneKind::Most(Box::new(most_view::MostPane::new(&list))),
            PaneSettings::Smart { symbol, tab } => {
                PaneKind::Smart(Box::new(smart_view::SmartPane::from_settings(&symbol, &tab)))
            }
            PaneSettings::Eco { min_importance, series } => {
                PaneKind::Eco(Box::new(eco_view::EcoPane::new(min_importance, series)))
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
    #[serde(default = "default_true")]
    sidebar: bool,
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
            if let Some(g) = &s.groups {
                app.groups = g.clone();
            }
            app.sidebar_open = s.sidebar;
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
            sidebar: self.sidebar_open,
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

    fn run_actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::OpenChart(s) => self.open_security(&s),
                Action::OpenChartNew(s) => self.open_chart_new(&s),
                Action::OpenTicker(s) => {
                    self.open_panel(PaneKind::Ticker { symbol: s, trades: Vec::new(), loading: true })
                }
                Action::OpenProfile(q) => self.open_panel(PaneKind::Profile { query: q, data: None, loading: true }),
                Action::AddToWatchlist(s) => {
                    let key = tr_data::security(&s).key();
                    if !self.watchlist.contains(&key) {
                        self.status = format!("Added {key} to the watchlist");
                        self.watchlist.push(key);
                        self.last_quotes = None;
                    } else {
                        self.status = format!("{key} is already on the watchlist");
                    }
                }
                Action::RemoveFromWatchlist(s) => {
                    let key = tr_data::security(&s).key();
                    self.watchlist.retain(|k| *k != key);
                    self.status = format!("Removed {key} from the watchlist");
                }
                Action::LinkedSymbol(link, s) => self.broadcast(link, &s, None),
                Action::AddAlert(spec) => self.add_alert(spec),
                Action::OpenAlertChart(id) => self.open_alert_chart(id),
                Action::AlertOp(op) => self.worker.send(Request::AlertRules(op)),
                Action::Command(line) => match command::parse(&line) {
                    Some(cmd) => self.dispatch(cmd),
                    None => tracing::warn!("internal command did not parse: {line}"),
                },
                Action::Track(q, on) => {
                    self.worker.send(Request::Track { query: q.clone(), on });
                    // Refresh any open profile so the star updates.
                    for p in &self.panes {
                        if let PaneKind::Profile { query, .. } = &p.kind {
                            self.worker.send(Request::Profile { pane: p.id, query: query.clone() });
                        }
                    }
                }
            }
        }
    }

    fn execute(&mut self, input: &str) {
        let Some(cmd) = command::parse(input) else {
            self.status = format!("Unknown command: {input}  (HELP lists functions)");
            return;
        };
        self.cmd_history.push(input.trim().to_string());
        self.dispatch(cmd);
    }

    fn dispatch(&mut self, cmd: Command) {
        match cmd {
            Command::Symbol(s) => self.open_security(&s),
            Command::SymbolFunction(s, Function::Congress) => {
                self.open_panel(PaneKind::Ticker { symbol: s.clone(), trades: Vec::new(), loading: true });
                self.status = format!("{s}: congressional trades");
            }
            Command::SymbolFunction(s, Function::News) => {
                self.open_panel(PaneKind::News(Box::new(NewsPane::new(Some(s.clone())))));
                self.status = format!("{s}: news");
            }
            Command::SymbolFunction(s, Function::OrderBook(depth)) => {
                let depth = depth.unwrap_or(self.prefs.panels.book_depth).clamp(5, 200);
                self.open_panel(PaneKind::OrderBook(Box::new(BookPane::new(&s, depth, 0.0))));
                self.status = format!("{s}: order book");
            }
            Command::SymbolFunction(s, Function::Options) => {
                self.open_panel(PaneKind::Options(Box::new(OptionsPane::new(&s, 12, true))));
                self.status = format!("{s}: option chain");
            }
            Command::SymbolFunction(s, Function::Earnings) => {
                self.open_earnings(Some(&s));
                self.status = format!("{s}: earnings history");
            }
            Command::SymbolFunction(s, Function::Portfolio(view, arg)) => self.portfolio_command(view, &arg, Some(s)),
            Command::SymbolFunction(s, Function::Trade(mut t)) => {
                t.symbol = Some(s);
                self.open_ticket(t, None);
            }
            Command::SymbolFunction(s, Function::Edgar { fa }) => {
                self.open_panel(PaneKind::Edgar(Box::new(edgar_view::EdgarPane::new(&s, fa))));
                self.status = format!("{s}: {}", if fa { "financial analysis" } else { "description" });
            }
            Command::SymbolFunction(s, Function::Etf(_)) => {
                let ticker = tr_data::security(&s).ticker;
                self.etf_command(&format!("HELD {ticker}"));
                self.status = format!("{ticker}: ETFs that hold it");
            }
            Command::SymbolFunction(s, Function::Smart(arg)) => self.smart_command(&arg, Some(s)),
            Command::SymbolFunction(s, Function::TrFunds(_)) => {
                let ticker = tr_data::security(&s).ticker;
                self.trf_held(&ticker);
                self.status = format!("{ticker}: funds that hold it");
            }
            Command::SymbolFunction(s, Function::AddAlert(mut a)) => {
                a.symbol.get_or_insert(s);
                self.add_alert(a);
            }
            Command::SymbolFunction(s, f @ (Function::NewPanel(_) | Function::Split { .. })) => {
                self.apply_with_symbol(f, Some(s));
            }
            Command::SymbolFunction(s, f) => {
                self.open_security(&s);
                self.apply(f);
            }
            Command::Function(f) => self.apply(f),
        }
    }

    fn apply(&mut self, f: Function) {
        self.apply_with_symbol(f, None);
    }

    fn apply_with_symbol(&mut self, f: Function, symbol: Option<String>) {
        match f {
            Function::Timeframe(tf) => self.with_chart(|c, w| c.set_timeframe(tf, w)),
            Function::ChartType(t) => self.with_chart(|c, _| c.chart_type = t),
            Function::Study(id) => self.with_chart(|c, w| c.toggle_study(&id, w)),
            Function::Indicator(spec) => self.with_chart(|c, w| c.toggle_indicator(spec, w)),
            Function::ClearIndicators => self.with_chart(|c, _| c.clear_indicators()),
            Function::Compare(s) => self.with_chart(|c, w| c.set_compare(s, w)),
            Function::LogScale => self.with_chart(|c, _| c.log = !c.log),
            Function::Range(r) => self.with_chart(|c, w| c.set_range(r, w)),
            Function::Draw(t) => self.with_chart(|c, _| c.start_tool(t)),
            Function::ClearDrawings => self.with_chart(|c, _| c.drawings.clear()),
            Function::Magnet => self.with_chart(|c, _| c.magnet = !c.magnet),
            Function::Congress => self.open_panel(PaneKind::Feed { filter: FeedFilter::default() }),
            Function::Politician(q) if q.trim().is_empty() => self.open_panel(PaneKind::Politicians),
            Function::Politician(q) => self.open_panel(PaneKind::Profile { query: q, data: None, loading: true }),
            Function::Track(q) => self.worker.send(Request::Track { query: q, on: true }),
            Function::Untrack(q) => self.worker.send(Request::Track { query: q, on: false }),
            Function::Alerts => {
                self.worker.send(Request::Alerts);
                self.open_panel(PaneKind::Alerts);
            }
            Function::AddAlert(mut a) => {
                if a.symbol.is_none() {
                    a.symbol = symbol.or_else(|| self.focused_symbol());
                }
                self.add_alert(a);
            }
            Function::Refresh => self.worker.send(Request::RefreshCongress),
            Function::Watchlist => self.open_panel(PaneKind::Watchlist),
            Function::WatchAdd(words) => {
                for key in keys_from_words(&words) {
                    if !self.watchlist.contains(&key) {
                        self.watchlist.push(key);
                    }
                }
                self.last_quotes = None;
            }
            Function::WatchRemove(words) => {
                let keys = keys_from_words(&words);
                self.watchlist.retain(|s| !keys.contains(s));
            }
            Function::Layout(arg) => self.layout_command(&arg),
            Function::Layouts => {
                self.settings_tab = SettingsTab::Layouts;
                self.open_panel(PaneKind::Settings);
                self.status = format!("Layouts: {}", self.layout_names.join(" · "));
            }
            Function::NewPanel(spec) => {
                let kind = self.panel_from_spec(&spec, symbol.as_deref());
                match kind {
                    Some(kind) => {
                        let target = self.focused_visible();
                        let id = self.new_pane(kind);
                        self.dock_pane(id, target, crate::dock::Zone::Right);
                    }
                    None => self.status = format!("Unknown panel {spec:?}; try CHART, NEWS, BOOK, WEI, GC, CONG, WL"),
                }
            }
            Function::Split { down, panel } => match self.panel_from_spec(&panel, symbol.as_deref()) {
                Some(kind) => {
                    let target = self.focused_visible();
                    let id = self.new_pane(kind);
                    let zone = if down { crate::dock::Zone::Bottom } else { crate::dock::Zone::Right };
                    self.dock_pane(id, target, zone);
                }
                None => self.status = format!("Unknown panel {panel:?}"),
            },
            Function::ClosePane => self.close_pane(self.focus),
            Function::PopOut => self.pop_out(self.focus),
            Function::Link(link) => {
                let focus = self.focus;
                self.set_link(focus, link);
            }
            Function::News => self.open_panel(PaneKind::News(Box::new(NewsPane::new(self.focused_symbol())))),
            Function::OrderBook(depth) => {
                let sym = self.focused_symbol().unwrap_or_else(|| "BTCUSD Curncy".into());
                let depth = depth.unwrap_or(self.prefs.panels.book_depth).clamp(5, 200);
                self.open_panel(PaneKind::OrderBook(Box::new(BookPane::new(&sym, depth, 0.0))));
            }
            Function::Settings => self.open_panel(PaneKind::Settings),
            Function::Options => {
                let sym = self.optionable_symbol();
                self.open_panel(PaneKind::Options(Box::new(OptionsPane::new(&sym, 12, true))));
            }
            Function::Earnings => self.open_earnings(symbol.as_deref()),
            Function::EarningsMarkers => self.with_chart(|c, _| c.earnings.show = !c.earnings.show),
            Function::Sidebar(arg) => {
                let side = &mut self.prefs.appearance.sidebar_side;
                match arg.as_str() {
                    "LEFT" => *side = prefs::SidebarSide::Left,
                    "RIGHT" => *side = prefs::SidebarSide::Right,
                    "SWAP" | "FLIP" | "OTHER" => *side = side.other(),
                    "SHOW" | "ON" => self.sidebar_open = true,
                    "HIDE" | "OFF" => self.sidebar_open = false,
                    _ => self.sidebar_open = !self.sidebar_open,
                }
                self.save_prefs();
            }
            Function::Theme(t) => {
                let system = self.system_theme;
                self.theme_command(&t, system);
            }
            Function::Title(t) => {
                let focus = self.focus;
                if let Some(p) = self.pane_mut(focus) {
                    p.title = (!t.trim().is_empty()).then(|| t.trim().to_string());
                }
            }
            Function::Help => self.open_panel(PaneKind::Help),
            Function::Changelog => self.open_panel(PaneKind::Changelog),
            Function::Wei => {
                self.last_wei = None;
                self.open_panel(PaneKind::Wei);
            }
            Function::Curve => {
                self.worker.send(Request::YieldCurves);
                self.last_curve = Some(Instant::now());
                self.open_panel(PaneKind::Curve);
            }
            Function::Search(q) => {
                if !q.trim().is_empty() {
                    self.worker.send(Request::Search(q.clone()));
                }
                self.open_panel(PaneKind::Search { query: q });
            }
            Function::Portfolio(view, arg) => self.portfolio_command(view, &arg, None),
            Function::Trade(t) => self.open_ticket(t, None),
            Function::AccountMode(live) => {
                let mode = if live { prefs::AccountMode::Live } else { prefs::AccountMode::Demo };
                if mode == self.prefs.trading.account_mode {
                    self.status = format!("Already using the {} account", if live { "LIVE" } else { "DEMO" });
                }
                self.set_account_mode(mode, false);
            }
            Function::Edgar { fa } => {
                let sym = self.focused_symbol().unwrap_or_default();
                self.open_panel(PaneKind::Edgar(Box::new(edgar_view::EdgarPane::new(&sym, fa))));
            }
            Function::Eco(arg) => self.eco_command(&arg),
            Function::EcoSeries(name) => self.eco_series_command(&name),
            Function::Etf(arg) => self.etf_command(&arg),
            Function::TrFunds(arg) => self.trf_command(&arg),
            Function::Most(arg) => self.most_command(&arg),
            Function::Smart(arg) => self.smart_command(&arg, None),
        }
    }

    /// Arm a price/indicator alert from the command line or a chart.
    fn add_alert(&mut self, a: crate::alerts::Spec) {
        let Some(key) = a.symbol.clone() else {
            self.status = "ALRT needs a security: ALRT AAPL > 250 (or focus a chart)".into();
            return;
        };
        let sec = tr_data::security(&key);
        let mode = if a.repeat { "repeating" } else { "once" };
        self.status = format!("Alert set: {} {} ({mode}) — ALRT lists rules", sec.ticker, a.cond.label());
        if let Some(q) = self.quotes.get(&key) {
            let m = crate::alerts::Market { last: Some(q.last), prev_close: Some(q.prev_close), ..Default::default() };
            if a.cond.state(&m) == Some(true) && a.cond.triggers(None, true) {
                self.status += " — already met, fires on the next quote";
            }
        }
        self.worker.send(Request::AlertRules(crate::alerts::RuleOp::Add(a)));
    }

    /// Unread Congress and price alerts (the ALERTS badge).
    pub(super) fn unread_alerts(&self) -> usize {
        self.alerts.iter().filter(|a| !a.seen).count() + self.price_alerts.unseen()
    }

    // ---- worker responses ----------------------------------------------------------------

    fn chart_mut(&mut self, pane: PaneId) -> Option<&mut ChartState> {
        match self.panes.iter_mut().find(|p| p.id == pane).map(|p| &mut p.kind) {
            Some(PaneKind::Chart(c)) => Some(c),
            _ => None,
        }
    }

    fn drain(&mut self) {
        while let Ok(r) = self.worker.rx.try_recv() {
            match r {
                Response::History {
                    pane,
                    generation,
                    symbol,
                    tf,
                    bars,
                    gpu,
                    prepended,
                    from_cache,
                    exhausted,
                    live,
                } => {
                    let worker = &self.worker;
                    if let Some(PaneKind::Chart(c)) = self.panes.iter_mut().find(|p| p.id == pane).map(|p| &mut p.kind)
                        && c.symbol == symbol
                        && c.tf == tf
                    {
                        c.on_history(generation, bars, gpu, prepended, from_cache, exhausted, live, worker);
                    }
                }
                Response::HistoryError { pane, generation, error } => {
                    // Errors while paging just mean "no older history"; only report failed loads.
                    let empty = self.chart_mut(pane).map(|c| {
                        c.on_history_error(generation, error.clone());
                        c.is_empty()
                    });
                    if empty == Some(true) {
                        self.status = format!("data error: {error}");
                    }
                }
                Response::Indicators { pane, generation, built } => {
                    if let Some(c) = self.chart_mut(pane) {
                        c.on_indicators(generation, built);
                    }
                }
                Response::Compare { pane, generation, symbol, line } => {
                    if let Some(c) = self.chart_mut(pane) {
                        c.on_compare(generation, &symbol, line);
                    }
                }
                Response::Studies { pane, generation, built } => {
                    if let Some(c) = self.chart_mut(pane) {
                        c.on_studies(generation, built);
                    }
                }
                Response::Earnings(r) => self.on_earnings(r),
                Response::Markers { pane, symbol, trades } => {
                    if let Some(c) = self.chart_mut(pane) {
                        c.on_markers(&symbol, trades);
                    }
                }
                Response::Quotes(q) => {
                    if q.len() > 20 {
                        self.wei_updated = Some(format!("updated {}", chrono::Local::now().format("%H:%M:%S")));
                    }
                    let keys: Vec<String> = q.iter().map(|x| x.symbol.clone()).collect();
                    for x in q {
                        self.quotes.insert(x.symbol.clone(), x);
                    }
                    self.demo_on_quotes(&keys);
                }
                Response::Latest(t) => self.latest = t,
                Response::TickerTrades { pane, symbol, trades } => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane)
                        && let PaneKind::Ticker { symbol: s, trades: t, loading } = &mut p.kind
                        && *s == symbol
                    {
                        *t = trades;
                        *loading = false;
                    }
                }
                Response::Profile { pane, profile, query } => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane)
                        && let PaneKind::Profile { query: q, data, loading } = &mut p.kind
                        && *q == query
                    {
                        *data = profile;
                        *loading = false;
                    }
                }
                Response::Options { pane, symbol, chain } => {
                    // Greeks use the 3-month T-bill yield when the curve is loaded.
                    let rate = self.curves.as_ref().and_then(|c| c.current.get("US3M")).map_or(0.04, |y| y / 100.0);
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane)
                        && let PaneKind::Options(o) = &mut p.kind
                        && o.symbol == symbol
                    {
                        match chain {
                            Ok(c) => o.set_chain(c, rate, chrono::Utc::now().timestamp()),
                            Err(e) => {
                                o.error = Some(e);
                                o.loading = false;
                            }
                        }
                    }
                }
                Response::News { pane, symbol, items } => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane)
                        && let PaneKind::News(n) = &mut p.kind
                        && n.symbol == symbol
                    {
                        n.loading = false;
                        match items {
                            Ok(items) => {
                                n.items = items;
                                n.error = None;
                            }
                            Err(e) => n.error = Some(e),
                        }
                    }
                }
                Response::OrderBook { pane, symbol, book } => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane)
                        && let PaneKind::OrderBook(b) = &mut p.kind
                        && b.symbol == symbol
                    {
                        match book {
                            Ok(book) => {
                                b.book = Some(book);
                                b.error = None;
                            }
                            Err(e) => b.error = Some(e),
                        }
                    }
                }
                Response::Portfolio(result) => {
                    let result = self.keep_simulated_trades(result);
                    self.on_portfolio(result)
                }
                Response::BrokerRegistered(result) => self.on_broker_registered(result),
                Response::BrokerPortal(result) => self.on_broker_portal(result),
                Response::BrokerDisconnected(result) => self.on_broker_disconnected(result),
                Response::Politicians(list) => self.politicians = list,
                Response::Alerts(a) => self.alerts = a,
                Response::NewAlerts(new) => {
                    for a in new {
                        tracing::info!("ALERT: {}", a.message);
                        self.toasts.push((a.message, Instant::now()));
                    }
                    self.worker.send(Request::Alerts);
                }
                Response::AlertBook(b) => self.price_alerts = *b,
                Response::PriceAlerts(fired) => {
                    for f in fired {
                        tracing::info!("PRICE ALERT: {}", f.message);
                        self.status = format!("Alert: {}", f.message);
                        self.toasts.push((format!("{}{}", crate::alerts::TOAST_PREFIX, f.message), Instant::now()));
                    }
                }
                Response::Status(s) => self.status = s,
                Response::Curves(c) => self.curves = Some(c),
                Response::SearchHits { query, hits } => self.net_hits = (query, hits),
                Response::Trade(r) => self.on_trade(*r),
                Response::CongressRefreshed(summary) => {
                    self.congress_status = summary.to_string();
                    self.congress_checked = Some(chrono::Local::now());
                    self.status = format!("Congress disclosures updated: {summary}");
                    self.refresh_congress_views();
                }
                Response::Edgar { pane, symbol, company } => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane)
                        && let PaneKind::Edgar(e) = &mut p.kind
                        && e.symbol == symbol
                    {
                        e.set_result(company);
                    }
                }
                Response::Bist { pane, symbol, usd, company } => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == pane)
                        && let PaneKind::Edgar(e) = &mut p.kind
                        && e.symbol == symbol
                    {
                        e.set_bist_result(usd, company);
                    }
                }
                Response::Eco { pane, resp } => self.on_eco(pane, resp),
                Response::Etf(resp) => self.on_etf(resp),
                Response::TrFunds(resp) => self.on_trfunds(resp),
                Response::Most(resp) => self.on_most(resp),
                Response::Smart(resp) => self.on_smart(resp),
            }
        }
    }

    /// Freshness line for the Congress feed: last check, newest filing, next check.
    fn congress_freshness(&self) -> String {
        let newest = self.latest.iter().map(|t| t.filed_date).max();
        let minutes = std::env::var("TR_CONGRESS_REFRESH_MIN")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(crate::worker::CONGRESS_REFRESH_MIN as i64);
        let mut parts = Vec::new();
        match self.congress_checked {
            Some(t) => {
                parts.push(format!("checked {}", t.format("%H:%M")));
                if minutes > 0 {
                    parts.push(format!("next {}", (t + chrono::Duration::minutes(minutes)).format("%H:%M")));
                }
            }
            None => parts.push("checking Senate eFD + House Clerk…".into()),
        }
        if let Some(d) = newest {
            parts.push(format!("newest filing {d}"));
        }
        if !self.congress_status.is_empty() {
            parts.push(self.congress_status.clone());
        }
        parts.join(" · ")
    }

    fn refresh_congress_views(&mut self) {
        self.worker.send(Request::Latest { limit: 2_000 });
        self.worker.send(Request::SearchPoliticians(String::new()));
        for p in &self.panes {
            match &p.kind {
                PaneKind::Ticker { symbol, .. } => {
                    self.worker.send(Request::TickerTrades { pane: p.id, symbol: symbol.clone() })
                }
                PaneKind::Chart(c) => self.worker.send(Request::Markers { pane: p.id, symbol: c.symbol.clone() }),
                _ => {}
            }
        }
        self.last_feed = Some(Instant::now());
    }

    // ---- input ---------------------------------------------------------------------------

    /// Global and chart hotkeys. Runs before the command line so it can claim keys; when
    /// the command line holds text, keys belong to it.
    fn hotkeys(&mut self, ctx: &egui::Context) {
        let presses: Vec<(Key, Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    Event::Key { key, pressed: true, repeat: false, modifiers, .. } if !prefs::is_modifier(*key) => {
                        Some((*key, *modifiers))
                    }
                    _ => None,
                })
                .collect()
        });
        // Settings is capturing a new binding: the next key press is it.
        if let Some(action) = self.rebinding {
            if let Some(&(key, m)) = presses.first() {
                ctx.input_mut(|i| i.consume_key(m, key));
                self.rebinding = None;
                if key != Key::Escape {
                    let chord = prefs::Chord::from_event(key, m);
                    let displaced = self.prefs.hotkeys.bind(action, chord);
                    self.status = match displaced {
                        Some(d) => {
                            format!("{} bound to {} (removed from {})", action.label(), chord.display(), d.label())
                        }
                        None => format!("{} bound to {}", action.label(), chord.display()),
                    };
                    self.save_prefs();
                }
            }
            return;
        }
        // ⌘K (Ctrl+K) opens and closes the palette from anywhere, even from a text field.
        if let Some(&(key, m)) =
            presses.iter().find(|(k, m)| self.prefs.hotkeys.action_for(*k, *m) == Some(prefs::Action::CommandPalette))
        {
            ctx.input_mut(|i| i.consume_key(m, key));
            self.palette.toggle();
            return;
        }
        // While it is open, the palette owns the keyboard.
        if self.palette.open {
            return;
        }
        let cmd_id = egui::Id::new("command-line");
        let focused = ctx.memory(|m| m.focused());
        let cmd_focused = focused == Some(cmd_id);
        let other_text_focused = focused.is_some() && !cmd_focused;
        if other_text_focused {
            return;
        }
        // Chorded bindings (⌘, Ctrl, Alt) work even while the command line has text.
        let mut swallow_text = false;
        for &(key, m) in &presses {
            let Some(action) = self.prefs.hotkeys.action_for(key, m) else { continue };
            let chorded = m.command || m.ctrl || m.alt;
            if !chorded && !self.cmd.is_empty() {
                continue;
            }
            // ⇧B / ⇧S type letters: never while the command line is being typed into.
            let letter = !chorded && Self::types_text(key);
            if letter && cmd_focused {
                continue;
            }
            if !self.hotkey_applies(action) {
                continue;
            }
            ctx.input_mut(|i| i.consume_key(m, key));
            swallow_text |= letter;
            self.run_hotkey(action);
        }
        if !cmd_focused && !swallow_text {
            // Type-anywhere: capture keystrokes until the command line has focus.
            // Judge modifiers per key event: the Alt release can arrive in the same frame
            // as Alt+T, so the frame-level modifier state is unreliable.
            let typed: String = ctx.input(|i| {
                let mut chord = false;
                let mut out = String::new();
                for e in &i.events {
                    match e {
                        Event::Key { pressed: true, modifiers, .. } => {
                            chord = modifiers.alt || modifiers.ctrl || modifiers.command
                        }
                        Event::Text(t) if !chord => out.push_str(t),
                        _ => {}
                    }
                }
                out
            });
            if !typed.is_empty() {
                let starts = !self.cmd.is_empty()
                    || typed.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '^' || c == '?');
                if starts {
                    self.cmd.push_str(&typed.to_ascii_uppercase());
                    self.focus_cmd = true;
                }
            }
        }
        if !self.cmd.is_empty() || !self.focused_is_chart() {
            return;
        }
        let pressed = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        if pressed(Modifiers::NONE, Key::Escape) {
            self.with_chart(|c, _| {
                c.cancel();
            });
        }
        if pressed(Modifiers::NONE, Key::Delete) || pressed(Modifiers::NONE, Key::Backspace) {
            self.with_chart(|c, _| {
                c.delete_selected();
            });
        }
        if pressed(Modifiers::NONE, Key::ArrowLeft) {
            self.with_chart(|c, _| c.pan_bars(-5.0));
        }
        if pressed(Modifiers::NONE, Key::ArrowRight) {
            self.with_chart(|c, _| c.pan_bars(5.0));
        }
        if pressed(Modifiers::COMMAND, Key::ArrowUp)
            || pressed(Modifiers::NONE, Key::Plus)
            || pressed(Modifiers::NONE, Key::Equals)
        {
            self.with_chart(|c, _| c.zoom(1.25));
        }
        if pressed(Modifiers::COMMAND, Key::ArrowDown) || pressed(Modifiers::NONE, Key::Minus) {
            self.with_chart(|c, _| c.zoom(0.8));
        }
    }

    /// Keys that also produce text (a bare or shifted letter, digit or symbol).
    fn types_text(key: Key) -> bool {
        let n = key.name();
        n.len() == 1 || matches!(key, Key::Space | Key::Minus | Key::Plus | Key::Equals | Key::Period | Key::Comma)
    }

    fn hotkey_applies(&self, a: prefs::Action) -> bool {
        use prefs::Action as A;
        match a {
            A::DrawTrendline
            | A::DrawHorizontal
            | A::DrawVertical
            | A::DrawFib
            | A::ResetView
            | A::LogScale
            | A::Magnet => self.focused_is_chart(),
            a => a.layout_slot().is_none_or(|i| i < self.layout_names.len()),
        }
    }

    fn run_hotkey(&mut self, a: prefs::Action) {
        use prefs::Action as A;
        match a {
            A::Help => self.apply(Function::Help),
            A::Congress => self.apply(Function::Congress),
            A::Politicians => self.apply(Function::Politician(String::new())),
            A::Alerts => self.apply(Function::Alerts),
            A::Watchlist => self.apply(Function::Watchlist),
            A::Refresh => self.apply(Function::Refresh),
            A::FrameTiming => self.show_perf = !self.show_perf,
            A::Portfolio => self.apply(Function::Portfolio(command::PortView::Summary, String::new())),
            A::Positions => self.apply(Function::Portfolio(command::PortView::Positions, String::new())),
            A::Orders => self.apply(Function::Portfolio(command::PortView::Orders, String::new())),
            A::Buy => self.quick_trade(true),
            A::Sell => self.quick_trade(false),
            A::NextPane => self.cycle_focus(1),
            A::PrevPane => self.cycle_focus(-1),
            A::ToggleSidebar => self.sidebar_open = !self.sidebar_open,
            A::NextTheme => self.cycle_theme(1),
            A::SwapSidebarSide => self.apply(Function::Sidebar("SWAP".into())),
            A::SplitRight => self.apply(Function::Split { down: false, panel: String::new() }),
            A::SplitDown => self.apply(Function::Split { down: true, panel: String::new() }),
            A::NewTab => {
                let target = self.focused_visible();
                let kind = self.panel_from_spec("", None).expect("chart");
                let id = self.new_pane(kind);
                self.dock_pane(id, target, crate::dock::Zone::Center);
            }
            A::ClosePane => self.close_pane(self.focus),
            A::PopOut => self.pop_out(self.focus),
            A::SaveLayout => {
                self.save_layout();
                self.status = format!("Saved layout {:?}", self.layout_name);
            }
            A::Settings => self.apply(Function::Settings),
            A::CommandPalette => self.palette.toggle(),
            A::DrawTrendline => self.with_chart(|c, _| c.start_tool(Tool::Trendline)),
            A::DrawHorizontal => self.with_chart(|c, _| c.start_tool(Tool::Horizontal)),
            A::DrawVertical => self.with_chart(|c, _| c.start_tool(Tool::Vertical)),
            A::DrawFib => self.with_chart(|c, _| c.start_tool(Tool::Fib)),
            A::ResetView => self.with_chart(|c, _| c.reset_view()),
            A::LogScale => self.with_chart(|c, _| c.log = !c.log),
            A::Magnet => self.with_chart(|c, _| c.magnet = !c.magnet),
            a => {
                if let Some(spec) = a.panel_spec() {
                    self.apply(Function::NewPanel(spec.into()));
                } else if let Some(name) = a.layout_slot().and_then(|i| self.layout_names.get(i)).cloned() {
                    self.switch_layout(&name);
                }
            }
        }
    }

    fn periodic(&mut self) {
        let visible = self.visible_panes();
        let shown = |f: &dyn Fn(&PaneKind) -> bool| visible.iter().any(|&i| f(&self.panes[i].kind));
        let visible_wl = self.sidebar_open || shown(&|k| matches!(k, PaneKind::Watchlist | PaneKind::List(_)));
        if self.last_quotes.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            let mut keys = Vec::new();
            if visible_wl {
                keys = self.watchlist.clone();
                let list_shown = |name: &str| shown(&|k| matches!(k, PaneKind::List(n) if n == name));
                for g in self.groups.iter().filter(|g| (g.open && self.sidebar_open) || list_shown(&g.name)) {
                    keys.extend(g.keys.iter().filter(|k| !keys.contains(k)).cloned().collect::<Vec<_>>());
                }
            }
            // Charted securities get quotes too, for the source and delay tag in the legend.
            for &i in &visible {
                if let PaneKind::Chart(c) = &self.panes[i].kind
                    && !keys.contains(&c.symbol)
                {
                    keys.push(c.symbol.clone());
                }
            }
            if !keys.is_empty() {
                self.worker.send(Request::Quotes(keys));
                self.last_quotes = Some(Instant::now());
            }
        }
        if shown(&|k| matches!(k, PaneKind::Wei)) && self.last_wei.is_none_or(|t| t.elapsed() > Duration::from_secs(10))
        {
            self.worker.send(Request::Quotes(markets_view::overview_keys()));
            self.last_wei = Some(Instant::now());
        }
        if shown(&|k| matches!(k, PaneKind::Curve))
            && self.last_curve.is_none_or(|t| t.elapsed() > Duration::from_secs(600))
        {
            self.worker.send(Request::YieldCurves);
            self.last_curve = Some(Instant::now());
        }
        // News every 5 minutes and order books every second, while visible.
        for &i in &visible {
            let p = &mut self.panes[i];
            match &mut p.kind {
                PaneKind::News(n) if n.fetched.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) => {
                    n.fetched = Some(Instant::now());
                    self.worker.send(Request::News { pane: p.id, symbol: n.symbol.clone() });
                }
                PaneKind::Options(o) if o.fetched.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) => {
                    o.fetched = Some(Instant::now());
                    self.worker.send(Request::Options {
                        pane: p.id,
                        symbol: o.symbol.clone(),
                        expiration: o.expiration,
                    });
                }
                PaneKind::OrderBook(b) if b.fetched.is_none_or(|t| t.elapsed() > Duration::from_millis(1000)) => {
                    b.fetched = Some(Instant::now());
                    self.worker.send(Request::OrderBook { pane: p.id, symbol: b.symbol.clone(), depth: b.depth });
                }
                PaneKind::Edgar(e) => {
                    if e.due(&self.prefs.panels.sec_user_agent) {
                        e.fetched = Some(Instant::now());
                        e.loading = true;
                        if e.bist_code().is_some() {
                            self.worker.send(Request::Bist { pane: p.id, symbol: e.symbol.clone(), usd: e.usd });
                        } else {
                            let user_agent = self.prefs.panels.sec_user_agent.clone();
                            self.worker.send(Request::Edgar { pane: p.id, symbol: e.symbol.clone(), user_agent });
                        }
                    }
                    if e.quote_due() {
                        e.quoted = Some(Instant::now());
                        self.worker.send(Request::Quotes(vec![tr_data::security(&e.symbol).key()]));
                    }
                }
                _ => {}
            }
        }
        self.broker_periodic(&visible);
        self.earnings_periodic(&visible);
        self.eco_periodic(&visible);
        self.etf_periodic(&visible);
        self.trf_periodic(&visible);
        self.most_periodic(&visible);
        self.smart_periodic(&visible);
        if let Some((q, at)) = &self.search_pending
            && at.elapsed() > Duration::from_millis(300)
        {
            self.worker.send(Request::Search(q.clone()));
            self.search_pending = None;
        }
        if self.last_feed.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
            self.worker.send(Request::Latest { limit: 2_000 });
            self.last_feed = Some(Instant::now());
        }
        self.save_dirty_themes(false);
        if self.last_save.elapsed() > Duration::from_secs(10) {
            self.autosave();
            self.last_save = Instant::now();
        }
        self.toasts.retain(|(_, t)| t.elapsed() < Duration::from_secs(10));
    }

    /// Save the active layout (only when it changed) and app settings.
    fn autosave(&mut self) {
        self.save_dirty_themes(true);
        let json = self.current_doc().to_json();
        if json != self.last_saved {
            self.save_layout();
        }
        self.save_settings();
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
