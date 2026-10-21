//! SEC EDGAR panes: security description (`DES`, see `des_view`) and financial analysis
//! (`FA`, see `fa_view`). Both share one pane kind, one fetch and one header.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{RichText, Sense, Ui};
use tr_data::Quote;
use tr_data::bist::{self, BistCompany};
use tr_data::edgar::{self, Company, Statement};

use crate::congress_view::Action;
use crate::layouts::PaneSettings;
use crate::prefs::PanelDefaults;
use crate::quote_tag;
use crate::theme::{self, amber, mono, muted, text, ui_font};
use crate::widgets;

/// Re-fetch loaded data after this long (the data layer caches on disk anyway).
const REFRESH: Duration = Duration::from_secs(30 * 60);
/// Retry a failed fetch after this long.
const RETRY: Duration = Duration::from_secs(60);

pub struct EdgarPane {
    pub symbol: String,
    /// `FA` (financial analysis) rather than `DES` (description).
    pub fa: bool,
    pub company: Option<Arc<Company>>,
    /// Borsa Istanbul securities load from İş Yatırım instead of SEC EDGAR.
    pub bist: Option<Arc<BistCompany>>,
    /// FA/DES on a BIST security: statements in USD rather than TRY.
    pub usd: bool,
    pub error: Option<String>,
    pub loading: bool,
    pub fetched: Option<Instant>,
    /// FA: quarterly rather than annual periods.
    pub quarterly: bool,
    /// FA: statement tab index into [`Statement::ALL`].
    pub tab: usize,
    /// FA: metric charted under the table.
    pub selected: Option<edgar::Metric>,
    /// Last live-quote request (price for market cap and multiples).
    pub quoted: Option<Instant>,
    /// Ticker box in the header while it is being edited.
    pub symbol_edit: Option<String>,
}

impl EdgarPane {
    pub fn new(symbol: &str, fa: bool) -> Self {
        Self {
            symbol: symbol.into(),
            fa,
            company: None,
            bist: None,
            usd: false,
            error: None,
            loading: false,
            fetched: None,
            quarterly: false,
            tab: 0,
            selected: None,
            quoted: None,
            symbol_edit: None,
        }
    }

    pub fn statement(&self) -> Statement {
        Statement::ALL[self.tab.min(Statement::ALL.len() - 1)]
    }

    pub fn title(&self) -> String {
        let t = tr_data::security(&self.symbol).ticker;
        if self.fa { format!("{t} Financials") } else { format!("{t} Description") }
    }

    pub fn code(&self) -> &'static str {
        if self.fa { "FA" } else { "DES" }
    }

    pub fn settings(&self) -> PaneSettings {
        PaneSettings::Edgar {
            symbol: self.symbol.clone(),
            fa: self.fa,
            quarterly: self.quarterly,
            tab: self.tab,
            usd: self.usd,
        }
    }

    pub fn from_settings(symbol: &str, fa: bool, quarterly: bool, tab: usize, usd: bool) -> Self {
        Self { quarterly, tab, usd, ..Self::new(symbol, fa) }
    }

    /// The Borsa Istanbul code when this is a BIST equity.
    pub fn bist_code(&self) -> Option<String> {
        bist::bist_ticker(&self.symbol)
    }

    /// Switch a BIST pane between TRY and USD statements (refetches; the data layer caches).
    pub fn set_usd(&mut self, usd: bool) {
        if self.usd != usd {
            self.usd = usd;
            self.bist = None;
            self.fetched = None;
            self.error = None;
        }
    }

    /// Follow a new security (link groups), keeping the view options.
    pub fn set_symbol(&mut self, symbol: &str) {
        if self.symbol != symbol {
            *self = Self { quarterly: self.quarterly, tab: self.tab, usd: self.usd, ..Self::new(symbol, self.fa) };
        }
    }

    /// Whether a (re)fetch is due. Securities EDGAR cannot cover, and a missing User-Agent,
    /// are reported in the pane without a request.
    pub fn due(&self, ua_setting: &str) -> bool {
        if self.loading || self.symbol.is_empty() {
            return false;
        }
        if self.bist_code().is_some() {
            let every = if self.error.is_some() { RETRY } else { REFRESH };
            return self.fetched.is_none_or(|t| t.elapsed() > every);
        }
        if edgar::edgar_ticker(&self.symbol).is_none() {
            return false;
        }
        if !edgar::user_agent(ua_setting).is_some_and(|u| edgar::valid_user_agent(&u)) {
            return false;
        }
        let every = if self.error.is_some() { RETRY } else { REFRESH };
        self.fetched.is_none_or(|t| t.elapsed() > every)
    }

    /// Whether to request a fresh quote (every 30 s while shown).
    pub fn quote_due(&self) -> bool {
        (edgar::edgar_ticker(&self.symbol).is_some() || self.bist_code().is_some())
            && self.quoted.is_none_or(|t| t.elapsed() > Duration::from_secs(30))
    }

    pub fn set_result(&mut self, company: Result<Arc<Company>, String>) {
        self.loading = false;
        match company {
            Ok(c) => {
                self.company = Some(c);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn set_bist_result(&mut self, usd: bool, company: Result<Arc<BistCompany>, String>) {
        self.loading = false;
        if usd != self.usd {
            // The currency was toggled while this was in flight: fetch again.
            self.fetched = None;
            return;
        }
        match company {
            Ok(c) => {
                self.bist = Some(c);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The live price for valuation, from the terminal's own quotes.
    pub fn price<'a>(&self, quotes: &'a HashMap<String, Quote>) -> Option<&'a Quote> {
        quotes.get(&tr_data::security(&self.symbol).key()).filter(|q| q.last > 0.0)
    }
}

/// What the pane asks of the app after drawing.
#[derive(Default)]
pub struct EdgarOut {
    pub actions: Vec<Action>,
    pub refetch: bool,
    /// The User-Agent was edited in the pane: save preferences.
    pub save_prefs: bool,
}

/// Draw a DES or FA pane.
pub fn edgar_pane(ui: &mut Ui, p: &mut EdgarPane, quotes: &HashMap<String, Quote>, d: &mut PanelDefaults) -> EdgarOut {
    let mut out = EdgarOut::default();
    header(ui, p, quotes, &mut out);
    ui.add_space(2.0);
    if p.symbol.is_empty() {
        note(
            ui,
            "Type a US or BIST ticker followed by DES or FA (e.g. AAPL DES, THYAO FA), or load a security into a \
             linked pane.",
        );
        return out;
    }
    if p.bist_code().is_some() {
        bist_body(ui, p, quotes, &mut out);
        return out;
    }
    if edgar::edgar_ticker(&p.symbol).is_none() {
        note(
            ui,
            &format!(
                "Not available: {} is neither a US-listed nor a Borsa Istanbul equity. DES and FA use SEC EDGAR \
                 filings (US companies) and İş Yatırım financials (Borsa Istanbul); indices, currencies, crypto, \
                 rates, futures and other foreign listings are not covered.",
                tr_data::security(&p.symbol).key()
            ),
        );
        return out;
    }
    if !edgar::user_agent(&d.sec_user_agent).is_some_and(|u| edgar::valid_user_agent(&u)) {
        ui.label(
            RichText::new(
                "SEC EDGAR asks every client to identify itself with a name and contact email. Enter yours \
                 (stored only in your local preferences, sent only to sec.gov):",
            )
            .font(ui_font(12.0))
            .color(text()),
        );
        if user_agent_field(ui, d) {
            out.save_prefs = true;
            out.refetch = true;
        }
        return out;
    }
    if let Some(e) = &p.error {
        // ETFs and other funds file as trusts, not under their ticker: point to the ETF pane.
        if e.contains("not an SEC registrant") {
            let t = tr_data::security(&p.symbol).ticker;
            note(
                ui,
                &format!(
                    "{t} has no company filings on SEC EDGAR. If it is an ETF or fund, its holdings are in the \
                     ETF pane; DES and FA cover operating companies."
                ),
            );
            ui.horizontal(|ui| {
                if ui.small_button(format!("Open ETF {t}")).clicked() {
                    out.actions.push(Action::Command(format!("ETF {t}")));
                }
                if ui.small_button("Retry").clicked() {
                    out.refetch = true;
                }
            });
            return out;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("EDGAR: {e}")).font(ui_font(12.0)).color(theme::down()));
            if ui.small_button("Retry").clicked() {
                out.refetch = true;
            }
        });
    }
    let Some(c) = p.company.clone() else {
        if p.error.is_none() {
            note(ui, "Loading SEC filings…");
        }
        return out;
    };
    if p.fa {
        let price = p.price(quotes).map(|q| q.last);
        super::fa_view::fa(ui, p, &c, price);
    } else {
        super::des_view::des(ui, &c, p.price(quotes));
    }
    out
}

/// DES / FA for a Borsa Istanbul security.
fn bist_body(ui: &mut Ui, p: &mut EdgarPane, quotes: &HashMap<String, Quote>, out: &mut EdgarOut) {
    if let Some(e) = &p.error {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("İş Yatırım: {e}")).font(ui_font(12.0)).color(theme::down()));
            if ui.small_button("Retry").clicked() {
                out.refetch = true;
                p.fetched = None;
            }
        });
    }
    let Some(c) = p.bist.clone() else {
        if p.error.is_none() {
            note(ui, "Loading Borsa Istanbul financials… (the first load of a company takes several seconds)");
        }
        return;
    };
    // Live TRY price; the USD view converts at the page's USD/TRY rate.
    let quote = p.price(quotes);
    let fx = c.profile.as_ref().ok().and_then(|pr| pr.usd_try);
    let price = match (quote.map(|q| q.last), c.currency) {
        (Some(x), bist::Currency::Try) => Some(x),
        (Some(x), bist::Currency::Usd) => fx.map(|r| x / r),
        (None, _) => None,
    };
    if p.fa {
        super::fa_view::fa_bist(ui, p, &c, price);
    } else {
        super::des_view::des_bist(ui, &c, price, quote);
    }
}

fn note(ui: &mut Ui, msg: &str) {
    ui.label(RichText::new(msg).font(ui_font(12.0)).color(muted()));
}

fn header(ui: &mut Ui, p: &mut EdgarPane, quotes: &HashMap<String, Quote>, out: &mut EdgarOut) {
    ui.horizontal_wrapped(|ui| {
        let key = tr_data::security(&p.symbol);
        symbol_box(ui, p, &key.ticker);
        if !p.symbol.is_empty() {
            let r = ui.label(RichText::new("↗").font(mono(13.0)).color(amber())).interact(Sense::click());
            if r.clicked() {
                out.actions.push(Action::OpenChart(p.symbol.clone()));
            }
            r.on_hover_text("Open chart");
        }
        if let Some(c) = &p.company {
            ui.label(RichText::new(&c.profile.name).font(ui_font(12.5)).color(text()));
        } else if let Some(Ok(pr)) = p.bist.as_ref().map(|c| &c.profile) {
            ui.label(RichText::new(&pr.name).font(ui_font(12.5)).color(text()));
        }
        if let Some(q) = p.price(quotes) {
            ui.label(RichText::new(format!("{:.2}", q.last)).font(mono(13.0)).color(text()));
            let ch = q.change_pct();
            let col = if ch >= 0.0 { theme::up() } else { theme::down() };
            ui.label(RichText::new(format!("{ch:+.2}%")).font(mono(12.0)).color(col));
            let now = quote_tag::now();
            if let Some(t) = quote_tag::tag(q, now) {
                ui.label(RichText::new(t.text).font(ui_font(10.5)).color(t.color))
                    .on_hover_text(quote_tag::tooltip(q, now));
            }
        }
        if p.loading {
            ui.add(egui::Spinner::new().size(12.0));
        }
        let sel = Some(usize::from(p.fa));
        if let Some(i) = widgets::segmented(ui, ("edgar-view", &p.symbol), &["DES", "FA"], sel, 20.0) {
            p.fa = i == 1;
        }
        if p.bist_code().is_some() {
            let sel = Some(usize::from(p.usd));
            if let Some(i) = widgets::segmented(ui, ("edgar-ccy", &p.symbol), &["TRY", "USD"], sel, 20.0) {
                p.set_usd(i == 1);
            }
        }
        if (p.company.is_some() || p.bist.is_some()) && ui.small_button("Refresh").clicked() {
            out.refetch = true;
        }
    });
}

/// Editable ticker: type a symbol and press Enter to load it; Escape keeps the old one.
fn symbol_box(ui: &mut Ui, p: &mut EdgarPane, ticker: &str) {
    let mut buf = p.symbol_edit.take().unwrap_or_else(|| ticker.to_string());
    let r = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .font(mono(13.0))
            .text_color(amber())
            .hint_text("AAPL / THYAO")
            .desired_width(theme::dp(72.0)),
    );
    let r = r.on_hover_text("Type a US or BIST ticker and press Enter");
    if r.lost_focus() {
        let entered = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let typed = buf.trim().to_ascii_uppercase();
        if entered && !typed.is_empty() {
            let key = tr_data::security(&typed).key();
            p.set_symbol(&key);
        }
    } else if r.has_focus() {
        p.symbol_edit = Some(buf);
    }
}

/// The SEC User-Agent text field (Settings › Panels, and the pane's first-run prompt).
/// Returns whether the value changed.
fn user_agent_field(ui: &mut Ui, d: &mut PanelDefaults) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut d.sec_user_agent)
                .hint_text("Jane Doe jane@example.com")
                .desired_width(280.0),
        );
        changed = r.changed();
        let env = std::env::var(edgar::UA_ENV).ok().filter(|v| !v.trim().is_empty());
        let status = match edgar::user_agent(&d.sec_user_agent) {
            Some(u) if edgar::valid_user_agent(&u) => (
                if d.sec_user_agent.trim().is_empty() { format!("using {}", edgar::UA_ENV) } else { "ok".into() },
                theme::up(),
            ),
            Some(_) => ("needs a contact email".into(), theme::down()),
            None if env.is_none() => (format!("or set {}", edgar::UA_ENV), muted()),
            None => (String::new(), muted()),
        };
        ui.label(RichText::new(status.0).font(ui_font(11.5)).color(status.1));
    });
    changed
}

/// Settings › Panels section for SEC EDGAR.
pub(super) fn sec_settings(ui: &mut Ui, d: &mut PanelDefaults) {
    ui.add_space(theme::dp(10.0));
    ui.label(RichText::new("SEC EDGAR (DES, FA)").font(ui_font(13.5)).color(text()));
    ui.separator();
    ui.horizontal(|ui| {
        ui.add_sized([140.0, 20.0], egui::Label::new(RichText::new("User-Agent").font(ui_font(12.5))));
        user_agent_field(ui, d);
    });
    ui.label(
        RichText::new(
            "SEC fair-access rules require a name and contact email on every request. Empty uses TR_SEC_USER_AGENT.",
        )
        .font(ui_font(11.5))
        .color(muted()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let mut p = EdgarPane::new("AAPL US Equity", true);
        p.quarterly = true;
        p.tab = 2;
        let json = serde_json::to_string(&p.settings()).unwrap();
        let back: PaneSettings = serde_json::from_str(&json).unwrap();
        let PaneSettings::Edgar { symbol, fa, quarterly, tab, usd } = back else { panic!("{json}") };
        let q = EdgarPane::from_settings(&symbol, fa, quarterly, tab, usd);
        assert_eq!(
            (q.symbol.as_str(), q.fa, q.quarterly, q.statement()),
            ("AAPL US Equity", true, true, Statement::CashFlow)
        );
        // Older layouts without the optional fields still load.
        let old: PaneSettings = serde_json::from_str(r#"{"Edgar":{"symbol":"MSFT US Equity"}}"#).unwrap();
        assert!(matches!(old, PaneSettings::Edgar { fa: false, tab: 0, .. }));
    }

    #[test]
    fn fetch_gating() {
        let ua = "Jane Doe jane@example.com";
        let mut p = EdgarPane::new("AAPL US Equity", false);
        assert!(p.due(ua));
        assert!(!EdgarPane::new("BTCUSD Curncy", false).due(ua), "not a US equity");
        assert!(!EdgarPane::new("", false).due(ua));
        if std::env::var(edgar::UA_ENV).is_err() {
            assert!(!p.due(""), "no User-Agent");
            assert!(!p.due("no-email"), "no contact");
        }
        p.fetched = Some(Instant::now());
        assert!(!p.due(ua));
        p.set_symbol("MSFT US Equity");
        assert!(p.due(ua) && p.company.is_none());
    }
}
