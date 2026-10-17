//! What's new (`CHANGES`): the user-facing changes, newest first.

use egui::{RichText, Ui};

use super::TerminalApp;
use crate::theme::{amber, dp, mono, muted, text, ui_font};

/// Release days, newest first: date, headline, items.
const ENTRIES: &[(&str, &str, &[&str])] = &[
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
            "MOST: US day gainers, losers and most active stocks from Yahoo screeners with volume, relative volume and market cap; click a row for its chart.",
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
