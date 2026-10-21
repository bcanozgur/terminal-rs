//! What's new (`CHANGES`): the user-facing changes, newest first.

use egui::{RichText, Ui};

use super::TerminalApp;
use crate::theme::{amber, dp, mono, muted, text, ui_font};

/// Release days, newest first: date, headline, items.
const ENTRIES: &[(&str, &str, &[&str])] = &[
    (
        "2026-10-22",
        "Fixes, data quality and search",
        &[
            "TOP: market news from CNBC, MarketWatch, Bloomberg HT, AA, CoinDesk, The Block, the Fed and the ECB in \
             World, Türkiye, Crypto and Central Banks tabs plus a merged All tab; watchlist tickers are highlighted \
             with a chip, and a click reads the story in the pane.",
            "ECO is global: EUR, GBP, JPY, CNY, CAD, AUD, NZD and CHF events (importance, forecast, previous) next \
             to the US releases, with a country column and chips (ECO US EU GB JP CN, ECO ALL for all). Note that \
             ECO ALL now means all countries; importance is ECO HIGH / MED / LOW. Only the current week (and \
             next or last on weekends) has world data; the pane says so for other weeks.",
            "KAP: Borsa Istanbul public disclosures from kap.org.tr, newest first. Filter to your watchlists and \
             positions, a category (dividend, buyback, capital, financial report...) or one company (THYAO KAP); \
             read a disclosure inside the pane and get a toast for new ones of your companies.",
            "Search button on the TR Funds, ETF and Economic Calendar pane headers: filters the fund list, every \
             holding of the open fund (not only the top 10), its buys and sells, reverse search hits and overlap \
             rows.",
            "The sidebar watchlist folds like BIST 30: click its header; the choice is remembered.",
            "TRF: reads more managers' reports (garbled Turkish letters, reports split over several PDFs, rows with \
             only nominal and value), fixes fund sizes read from the wrong line and labels weekly reports \
             (Week 41 2026). A scanned report without text now says so.",
            "ETF: issuer names matched to tickers far more often (VOO 498 of 513), with share classes kept apart \
             (GOOGL / GOOG, TAP / TAP-A).",
            "OMON: Yahoo's placeholder IVs are dropped and solved from the quote instead (shown as ≈); the default \
             expiry skips a same-day expiry that has already closed.",
            "RISK: series in another currency (XU100 in TRY) are converted at daily FX rates instead of mixed in as \
             is.",
            "FUT: expired contracts no longer show as the front month.",
            "MOST: says whether the market is open, pre-market or after hours, and which session the changes are \
             from.",
            "ERN: report history falls back to the calendar when Nasdaq's history is empty, sessions come from \
             EDGAR filing times and EPS is adjusted for stock splits.",
            "ECO and the yield curve load again (FRED stopped answering the app's requests); market status names \
             the exchange's time zone; chart alert levels are rounded to the instrument's precision.",
        ],
    ),
    (
        "2026-10-16",
        "Research, data quality and alerts",
        &[
            "Research section in the sidebar (Description, Financials, Earnings, Economic Calendar); the same panels \
             in every + menu, and Research ▸ DES / FA on a stock's right-click menu.",
            "DES and FA: company profile, statements and ratios for US companies (SEC EDGAR) and Borsa Istanbul \
             companies (İş Yatırım). Type a ticker in the pane's box to switch company.",
            "BIST financials: TRY / USD switch, banks on their own template, and year-to-date periods (3M/6M/9M) as \
             published, because inflation accounting makes derived quarters unreliable.",
            "ERN: weekly earnings calendar (before the open / after the close, EPS estimates), each company's report \
             history, and E markers on the chart.",
            "ECO: US economic calendar with consensus, actual, prior and change; click a release for its history \
             (ECOF). Needs a free FRED API key.",
            "ALRT: price, % change and indicator alerts, also from the chart's right-click menu.",
            "Pre-market and after-hours prices for US stocks in quotes and the portfolio.",
            "Every price shows its source and delay (e.g. Yahoo · 15m delayed, Binance · live); stale prices are \
             flagged and simulated data shows a red warning.",
            "ETF: US ETF contents from SEC N-PORT and Yahoo. Click a fund for a top-10 donut chart and list plus \
             sector weights; reverse search (which ETFs hold a stock) and two-fund overlap.",
            "TRF: Turkish mutual funds from KAP. Search the fund list, open a fund for its card, asset and stock \
             allocation donuts and the buys and sells between the latest two monthly reports; reverse search \
             (which funds hold a BIST stock, also THYAO TRFS).",
            "Order safety: pre-trade checks with limits (Settings › Trading), a KILL switch in the toolbar (KILL / \
             KILL OFF), an append-only AUDIT log with CSV export, and a market holiday calendar (HOL) that warns \
             when you order while the exchange is closed.",
            "CRYP: crypto overview with market cap, dominance, stablecoins, Fear & Greed, Binance funding, open interest and long/short, 24h movers and the BtcTurk USDT/TRY premium; click a coin for its chart.",
            "MOST: US day gainers, losers and most active stocks from Yahoo screeners with volume, relative volume and market cap; click a row for its chart.",
            "FUT / VIX / HEAT: futures curves (contango, backwardation, roll yield), VIX term structure with the Cboe VX settlement strip, and a sector / world heatmap by 1D, 1W, 1M and YTD.",
            "SMART: insider trades (SEC Form 4) with a 90-day net buy/sell summary, 13F holdings of ~18 famous \
             managers (top positions, new/added/reduced/sold, who holds the symbol) and FINRA short interest with \
             days to cover; also AAPL SMART.",
            "RISK / CORR: portfolio risk from daily history of your positions: volatility, VaR and expected shortfall, \
             beta, alpha, Sharpe/Sortino against SPY, QQQ or XU100, drawdown chart, risk contribution per position \
             and a correlation heatmap.",
            "Sync button on symbol panes (News, Options, DES/FA, Order Book, ...) shows the active chart's symbol; \
             Options now has a ticker box too.",
            "Chart alerts: click the price axis for a small popup, amber line across the chart, and the alert keeps \
             its timeframe; click an ALRT row to open that chart.",
            "Portfolio positions table has a Day P&L column.",
            "News: click a headline to read the article inside the pane (Back returns to the list); Open in \
             Browser stays one click away.",
            "Options analytics: Chain | Analytics | Payoff tabs in OMON. IV smile and 25Δ skew, ATM IV term structure, \
             put/call ratios by volume and open interest, max pain and call/put walls, and a payoff builder \
             (presets or click a chain row) with break-evens, max profit/loss and position greeks.",
            "Chart: ATR, Stochastic, ADX/DI, Ichimoku Cloud, OBV, anchored VWAP (AVWAP, or right-click a bar) and a \
             volume profile with POC and 70% value area; new drawings: rectangle, ray, parallel channel, text note, \
             measure and long / short position (risk:reward). Drag a selected drawing to move it, right-click it for \
             colours. Watchlist: right-click the header for RelVol, RSI, ATR%, market cap, 52W position and volume \
             columns; click a header to sort.",
            "Info (i) button on every pane header explains what the panel shows.",
            "Combo 11 chart study, with its table kept clear of the latest bars.",
            "Default zoom setting, search inside list panels, app icon, and a toolbar flush with the top in full \
             screen.",
        ],
    ),
    (
        "2026-10-13",
        "Trading",
        &["Demo (paper) account by default with one-click Buy / Sell; live trading through SnapTrade is opt-in."],
    ),
    (
        "2026-10-12",
        "Workspace and markets",
        &[
            "Docking workspace: drag panes, tabs, saved layouts, colour link groups and pop-out windows.",
            "Cmd+K command palette, themes (built-in and custom), sidebar on either side.",
            "Panels for world indices, yield curve, news, order book, option chains (OMON) and watchlists.",
            "Native macOS look with a unified toolbar and translucent sidebar.",
            "Brokerage accounts through SnapTrade (Settings ▸ Brokerage).",
        ],
    ),
    (
        "2026-10-11",
        "First version",
        &[
            "GPU charts that handle millions of bars, with indicators (SMA, RSI, MACD and more).",
            "Bloomberg-style command line: <SECURITY> <FUNCTION> <GO> and hotkeys.",
            "Congress trades: Senate and House disclosures, politician profiles and filing alerts.",
        ],
    ),
];

impl TerminalApp {
    pub(super) fn changelog_panel(&mut self, ui: &mut Ui) {
        let width = ui.available_width() - 14.0;
        egui::ScrollArea::vertical().id_salt("changelog").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(width);
            ui.label(RichText::new("What's New").font(ui_font(19.0)).color(text()));
            for (date, title, items) in ENTRIES {
                ui.add_space(dp(14.0));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(*date).font(mono(12.5)).color(amber()));
                    ui.label(RichText::new(*title).font(ui_font(14.0)).color(text()));
                });
                ui.add_space(dp(2.0));
                for item in *items {
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new("•").font(ui_font(12.5)).color(muted()));
                        ui.add(egui::Label::new(RichText::new(*item).font(ui_font(12.5)).color(muted())).wrap());
                    });
                }
            }
            ui.add_space(dp(12.0));
        });
    }
}
