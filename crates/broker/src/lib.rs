//! Brokerage integration through [SnapTrade](https://snaptrade.com): a signed API client, a
//! normalised portfolio model (accounts, balances, positions, orders, activity), secure
//! credential storage and an offline simulated portfolio.
//!
//! Order entry is in [`trading`]: every order is previewed with SnapTrade's order-impact check
//! before it can be placed, and the terminal keeps trading off unless the user enables it.
//! Besides orders and registering a user, the only write is removing a brokerage connection
//! when the user explicitly asks to disconnect it.

pub mod model;
pub mod sample;
pub mod secrets;
pub mod snaptrade;
pub mod trading;

pub use model::{Account, Activity, Connection, InstrumentKind, Order, Portfolio, Position, Totals};
pub use secrets::{Credentials, SecretStore};
pub use snaptrade::SnapTrade;
