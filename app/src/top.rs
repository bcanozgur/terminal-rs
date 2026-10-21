//! Worker side of the TOP pane: one request fetches every feed concurrently, so a slow or dead
//! feed only costs its own rows; article text goes through the shared article reader.

use tr_data::article::Article;
use tr_data::topnews::{self, TopItem};

use crate::worker::PaneId;

pub enum TopReq {
    Load { pane: PaneId },
    Article { pane: PaneId, url: String },
}

pub enum TopResp {
    /// One result per feed, in `topnews::FEEDS` order.
    Feeds { pane: PaneId, feeds: Vec<Result<Vec<TopItem>, String>> },
    Article { pane: PaneId, url: String, article: Result<Article, String> },
}

pub async fn handle(req: TopReq, send: &(dyn Fn(TopResp) + Sync)) {
    match req {
        TopReq::Load { pane } => {
            let feeds = futures::future::join_all(topnews::FEEDS.iter().map(|f| async move {
                topnews::fetch_feed(f).await.map_err(|e| format!("{e:#}"))
            }))
            .await;
            send(TopResp::Feeds { pane, feeds });
        }
        TopReq::Article { pane, url } => {
            let article = tr_data::article::fetch(&url).await.map_err(|e| format!("{e:#}"));
            send(TopResp::Article { pane, url, article });
        }
    }
}
