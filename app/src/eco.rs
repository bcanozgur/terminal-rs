//! Worker side of the economic calendar (`ECO`/`ECOF`): requests, responses and the shared
//! FRED client with its SQLite cache (`<data dir>/eco.sqlite3`).

use std::sync::OnceLock;

use chrono::NaiveDate;
use tr_data::eco::{self, Eco, EcoCache, EcoSeries, EcoWeek, KeyStore};

pub enum EcoRequest {
    /// The calendar for the week starting on this Monday.
    Week { monday: NaiveDate },
    /// History of one event (`ECOF`).
    Series { code: String },
}

pub enum EcoResponse {
    Week(Box<EcoWeek>),
    Series { code: String, series: Result<Box<EcoSeries>, String> },
}

/// Where the FRED API key is stored (`FRED_API_KEY` overrides it).
pub fn key_store() -> KeyStore {
    KeyStore::new(&crate::worker::data_dir())
}

fn client() -> Option<&'static Eco> {
    static CLIENT: OnceLock<Option<Eco>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let cache = EcoCache::open(crate::worker::data_dir().join("eco.sqlite3"))
                .map_err(|e| tracing::warn!("eco cache unavailable: {e:#}"))
                .ok();
            Eco::new(cache).map_err(|e| tracing::warn!("eco client: {e:#}")).ok()
        })
        .as_ref()
}

pub async fn fetch(req: EcoRequest) -> EcoResponse {
    let now = chrono::Utc::now().timestamp();
    match req {
        EcoRequest::Week { monday } => {
            let week = match client() {
                Some(c) => c.week(key_store().key().as_deref(), monday, now).await,
                None => EcoWeek {
                    monday,
                    rows: eco::schedule(monday, &[]),
                    source: eco::DateSource::Unavailable,
                    latest: Vec::new(),
                    notes: vec!["HTTP client unavailable".into()],
                },
            };
            EcoResponse::Week(Box::new(week))
        }
        EcoRequest::Series { code } => {
            let series = match (eco::event(&code), client()) {
                (Some(ev), Some(c)) => c.series(ev, now).await.map(Box::new).map_err(|e| format!("{e:#}")),
                (None, _) => Err(format!("unknown release {code}")),
                (_, None) => Err("HTTP client unavailable".into()),
            };
            EcoResponse::Series { code, series }
        }
    }
}
