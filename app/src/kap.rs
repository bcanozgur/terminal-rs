//! Worker side of the KAP pane: the latest-disclosures feed, one company's disclosures and one
//! disclosure's text.

use tr_data::kap::{self, Detail, Disclosure};

use crate::worker::PaneId;

pub enum KapReq {
    /// The listed companies' disclosures of today and the `back` days before.
    Latest { pane: PaneId, back: u32 },
    Company { pane: PaneId, code: String },
    Detail { pane: PaneId, index: u64 },
}

pub enum KapResp {
    Latest { pane: PaneId, rows: Result<Vec<Disclosure>, String> },
    Company { pane: PaneId, code: String, rows: Result<Vec<Disclosure>, String> },
    Detail { pane: PaneId, index: u64, detail: Result<Detail, String> },
}

pub async fn handle(req: KapReq, send: &(dyn Fn(KapResp) + Sync)) {
    match req {
        KapReq::Latest { pane, back } => {
            let rows = kap::latest(back).await.map_err(|e| format!("{e:#}"));
            send(KapResp::Latest { pane, rows });
        }
        KapReq::Company { pane, code } => {
            let rows = kap::company(&code, kap::COMPANY_DAYS).await.map_err(|e| format!("{e:#}"));
            send(KapResp::Company { pane, code, rows });
        }
        KapReq::Detail { pane, index } => {
            let detail = kap::detail(index).await.map_err(|e| format!("{e:#}"));
            send(KapResp::Detail { pane, index, detail });
        }
    }
}
