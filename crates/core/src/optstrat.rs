//! Option-chain analytics and strategy maths: ATM IV, skew, put/call ratios, max pain,
//! open-interest walls, and multi-leg payoff (at expiry and Black-Scholes today), break-evens,
//! max profit / loss and aggregate greeks. Pure functions over plain numbers; anything the
//! data source did not provide stays `None` instead of being invented.

use crate::options::{Greeks, black_scholes};

/// Shares per listed US equity option contract.
pub const CONTRACT_SIZE: f64 = 100.0;

/// The fields of one listed option that the analytics read.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Quote {
    pub strike: f64,
    /// Annualised implied volatility (0.25 = 25%).
    pub iv: Option<f64>,
    /// Option delta (calls positive, puts negative).
    pub delta: Option<f64>,
    pub volume: Option<u64>,
    pub open_interest: Option<u64>,
}

/// Nearest-strike IV of one side, ignoring contracts without an IV.
fn nearest_iv(side: &[Quote], spot: f64) -> Option<f64> {
    side.iter()
        .filter(|q| q.iv.is_some_and(|v| v > 0.0))
        .min_by(|a, b| (a.strike - spot).abs().total_cmp(&(b.strike - spot).abs()))
        .and_then(|q| q.iv)
}

/// At-the-money IV: the mean of the call and put IV at the strikes nearest the spot.
pub fn atm_iv(calls: &[Quote], puts: &[Quote], spot: f64) -> Option<f64> {
    let v: Vec<f64> = [nearest_iv(calls, spot), nearest_iv(puts, spot)].into_iter().flatten().collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// How the skew wings were picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkewBasis {
    /// Put near -0.25Δ and call near +0.25Δ.
    Delta,
    /// Put near 95% and call near 105% of spot (no usable deltas).
    Moneyness,
}

/// Put-minus-call IV at comparable wings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Skew {
    pub basis: SkewBasis,
    pub put_strike: f64,
    pub put_iv: f64,
    pub call_strike: f64,
    pub call_iv: f64,
    /// `put_iv - call_iv` in volatility terms (0.03 = 3 vol points); positive = puts richer.
    pub skew: f64,
}

/// The quote with an IV whose `key` is closest to `target`, within `tol`.
fn closest(side: &[Quote], key: impl Fn(&Quote) -> Option<f64>, target: f64, tol: f64) -> Option<Quote> {
    side.iter()
        .filter(|q| q.iv.is_some_and(|v| v > 0.0))
        .filter_map(|q| key(q).map(|k| (q, (k - target).abs())))
        .filter(|(_, d)| *d <= tol)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(q, _)| *q)
}

/// 25-delta style skew, falling back to 95%/105% moneyness when deltas are missing or no
/// strike sits within 0.15Δ of the 0.25 target (wings of 0.10–0.40Δ). `None` when neither works.
pub fn skew(calls: &[Quote], puts: &[Quote], spot: f64) -> Option<Skew> {
    let mk = |basis, p: Quote, c: Quote| {
        let (put_iv, call_iv) = (p.iv?, c.iv?);
        Some(Skew { basis, put_strike: p.strike, put_iv, call_strike: c.strike, call_iv, skew: put_iv - call_iv })
    };
    let by_delta = closest(puts, |q| q.delta, -0.25, 0.15)
        .zip(closest(calls, |q| q.delta, 0.25, 0.15))
        .and_then(|(p, c)| mk(SkewBasis::Delta, p, c));
    by_delta.or_else(|| {
        let tol = 0.03 * spot;
        let p = closest(puts, |q| Some(q.strike), 0.95 * spot, tol)?;
        let c = closest(calls, |q| Some(q.strike), 1.05 * spot, tol)?;
        mk(SkewBasis::Moneyness, p, c)
    })
}

fn sum_opt<'a>(it: impl Iterator<Item = &'a Option<u64>>) -> Option<u64> {
    it.fold(None, |acc, v| match (acc, v) {
        (a, None) => a,
        (None, Some(v)) => Some(*v),
        (Some(a), Some(v)) => Some(a + v),
    })
}

fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Call and put totals; a total is `None` when no contract reported that field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PutCall {
    pub call_volume: Option<u64>,
    pub put_volume: Option<u64>,
    pub call_oi: Option<u64>,
    pub put_oi: Option<u64>,
}

fn ratio(put: Option<u64>, call: Option<u64>) -> Option<f64> {
    match (put, call) {
        (Some(p), Some(c)) if c > 0 => Some(p as f64 / c as f64),
        _ => None,
    }
}

impl PutCall {
    pub fn of(calls: &[Quote], puts: &[Quote]) -> Self {
        Self {
            call_volume: sum_opt(calls.iter().map(|q| &q.volume)),
            put_volume: sum_opt(puts.iter().map(|q| &q.volume)),
            call_oi: sum_opt(calls.iter().map(|q| &q.open_interest)),
            put_oi: sum_opt(puts.iter().map(|q| &q.open_interest)),
        }
    }

    /// Sum of two sets of totals (e.g. across expirations).
    pub fn plus(self, o: Self) -> Self {
        Self {
            call_volume: add_opt(self.call_volume, o.call_volume),
            put_volume: add_opt(self.put_volume, o.put_volume),
            call_oi: add_opt(self.call_oi, o.call_oi),
            put_oi: add_opt(self.put_oi, o.put_oi),
        }
    }

    /// Put volume / call volume.
    pub fn volume_ratio(&self) -> Option<f64> {
        ratio(self.put_volume, self.call_volume)
    }

    /// Put open interest / call open interest.
    pub fn oi_ratio(&self) -> Option<f64> {
        ratio(self.put_oi, self.call_oi)
    }
}

/// One-line reading of a put/call ratio. A rule of thumb, not a signal: puts are also hedges.
pub fn interpret_put_call(r: f64) -> &'static str {
    if r < 0.7 {
        "call-heavy: bullish positioning"
    } else if r <= 1.0 {
        "balanced"
    } else {
        "put-heavy: bearish positioning or hedging"
    }
}

/// Max-pain result: the settlement price that costs option writers the least.
#[derive(Debug, Clone, PartialEq)]
pub struct MaxPain {
    pub strike: f64,
    /// Total intrinsic value paid to holders at `strike`, in dollars.
    pub payout: f64,
    /// (settlement price, total payout in dollars) at every strike with open interest.
    pub curve: Vec<(f64, f64)>,
}

/// Strike minimising the total intrinsic value held by call and put holders, from open
/// interest. Ties go to the lowest strike. `None` without any open interest.
pub fn max_pain(calls: &[Quote], puts: &[Quote]) -> Option<MaxPain> {
    let oi = |q: &Quote| q.open_interest.unwrap_or(0) as f64;
    if calls.iter().chain(puts).map(oi).sum::<f64>() <= 0.0 {
        return None;
    }
    let mut strikes: Vec<f64> = calls.iter().chain(puts).filter(|q| oi(q) > 0.0).map(|q| q.strike).collect();
    strikes.sort_by(f64::total_cmp);
    strikes.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let curve: Vec<(f64, f64)> = strikes
        .iter()
        .map(|&s| {
            let c: f64 = calls.iter().map(|q| oi(q) * (s - q.strike).max(0.0)).sum();
            let p: f64 = puts.iter().map(|q| oi(q) * (q.strike - s).max(0.0)).sum();
            (s, (c + p) * CONTRACT_SIZE)
        })
        .collect();
    let (strike, payout) = curve.iter().copied().fold(None, |best: Option<(f64, f64)>, pt| match best {
        Some(b) if b.1 <= pt.1 => Some(b),
        _ => Some(pt),
    })?;
    Some(MaxPain { strike, payout, curve })
}

/// The `n` strikes with the most open interest (descending; ties by lower strike).
pub fn top_oi(side: &[Quote], n: usize) -> Vec<(f64, u64)> {
    let mut v: Vec<(f64, u64)> =
        side.iter().filter_map(|q| q.open_interest.filter(|&o| o > 0).map(|o| (q.strike, o))).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.total_cmp(&b.0)));
    v.truncate(n);
    v
}

// ---- Strategies ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegKind {
    Call,
    Put,
    /// 100 shares per lot (so a covered call is +1 stock, -1 call).
    Stock,
}

/// One leg: `qty` contracts (or 100-share lots), positive = long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leg {
    pub kind: LegKind,
    /// Strike (for stock: the entry price).
    pub strike: f64,
    /// Expiration, Unix seconds (ignored for stock).
    pub expiry: i64,
    pub qty: i32,
    /// Price paid or received per share at entry.
    pub premium: f64,
    /// IV used for the Black-Scholes value; `None` makes the today-curve n/a.
    pub iv: Option<f64>,
}

impl Leg {
    fn lots(&self) -> f64 {
        self.qty as f64 * CONTRACT_SIZE
    }

    fn intrinsic(&self, s: f64) -> f64 {
        match self.kind {
            LegKind::Call => (s - self.strike).max(0.0),
            LegKind::Put => (self.strike - s).max(0.0),
            LegKind::Stock => s,
        }
    }
}

/// Years from `now` to the 16:00 New York close (≈ 20:00 UTC) of the expiration date.
/// Zero or negative once expired.
pub fn years_to_expiry(expiry: i64, now: i64) -> f64 {
    (expiry + 20 * 3600 - now) as f64 / (365.0 * 86_400.0)
}

/// Net cost of opening the position in dollars: positive = debit paid, negative = credit.
pub fn net_premium(legs: &[Leg]) -> f64 {
    legs.iter().map(|l| l.lots() * l.premium).sum()
}

/// The common expiration of all option legs, if they share one (`None` = mixed or no options).
pub fn single_expiry(legs: &[Leg]) -> Option<i64> {
    let mut it = legs.iter().filter(|l| l.kind != LegKind::Stock).map(|l| l.expiry);
    let first = it.next()?;
    it.all(|e| e == first).then_some(first)
}

/// Profit or loss in dollars if the underlying is `s` when every option expires.
pub fn pnl_at_expiry(legs: &[Leg], s: f64) -> f64 {
    legs.iter().map(|l| l.lots() * (l.intrinsic(s) - l.premium)).sum()
}

/// Profit or loss in dollars if the underlying is `s` now, options valued by Black-Scholes at
/// their leg IV (intrinsic once expired). `None` if a live option lacks an IV or cannot be priced.
pub fn pnl_today(legs: &[Leg], s: f64, now: i64, r: f64, q: f64) -> Option<f64> {
    legs.iter().try_fold(0.0, |acc, l| {
        let value = if l.kind == LegKind::Stock {
            s
        } else {
            let t = years_to_expiry(l.expiry, now);
            if t <= 0.0 {
                l.intrinsic(s)
            } else {
                black_scholes(l.kind == LegKind::Call, s, l.strike, t, r, q, l.iv?)?.0
            }
        };
        Some(acc + l.lots() * (value - l.premium))
    })
}

/// Position greeks in dollar/share-equivalent terms (delta in shares, theta in $ per day,
/// vega in $ per vol point). Expired options contribute nothing. `None` if a live option has no IV.
pub fn position_greeks(legs: &[Leg], s: f64, now: i64, r: f64, q: f64) -> Option<Greeks> {
    let mut tot = Greeks::default();
    for l in legs {
        let g = if l.kind == LegKind::Stock {
            Greeks { delta: 1.0, ..Greeks::default() }
        } else {
            let t = years_to_expiry(l.expiry, now);
            if t <= 0.0 {
                continue;
            }
            black_scholes(l.kind == LegKind::Call, s, l.strike, t, r, q, l.iv?)?.1
        };
        let m = l.lots();
        tot.delta += m * g.delta;
        tot.gamma += m * g.gamma;
        tot.theta += m * g.theta;
        tot.vega += m * g.vega;
        tot.rho += m * g.rho;
    }
    Some(tot)
}

/// A bound that may be infinite.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Bound {
    Finite(f64),
    Unlimited,
}

/// Kink points of the expiry payoff (0 and every option strike), ascending.
fn kinks(legs: &[Leg]) -> Vec<f64> {
    let mut v: Vec<f64> =
        std::iter::once(0.0).chain(legs.iter().filter(|l| l.kind != LegKind::Stock).map(|l| l.strike)).collect();
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    v
}

/// Slope of the expiry payoff above the highest strike, in dollars per $1 of underlying.
fn tail_slope(legs: &[Leg]) -> f64 {
    legs.iter().filter(|l| l.kind != LegKind::Put).map(|l| l.lots()).sum()
}

/// Largest profit at expiry (underlying ≥ 0); `Unlimited` when the payoff rises without bound.
pub fn max_profit(legs: &[Leg]) -> Bound {
    if tail_slope(legs) > 1e-9 {
        return Bound::Unlimited;
    }
    Bound::Finite(kinks(legs).into_iter().map(|s| pnl_at_expiry(legs, s)).fold(f64::NEG_INFINITY, f64::max))
}

/// Largest loss at expiry as a negative P&L (or `Unlimited`).
pub fn max_loss(legs: &[Leg]) -> Bound {
    if tail_slope(legs) < -1e-9 {
        return Bound::Unlimited;
    }
    Bound::Finite(kinks(legs).into_iter().map(|s| pnl_at_expiry(legs, s)).fold(f64::INFINITY, f64::min))
}

/// Underlying prices (> 0) where the expiry payoff crosses zero, ascending.
pub fn breakevens(legs: &[Leg]) -> Vec<f64> {
    if legs.is_empty() {
        return Vec::new();
    }
    let pts = kinks(legs);
    let f: Vec<f64> = pts.iter().map(|&s| pnl_at_expiry(legs, s)).collect();
    let scale = f.iter().fold(1.0f64, |m, v| m.max(v.abs()));
    let eps = 1e-9 * scale;
    let mut out: Vec<f64> = Vec::new();
    let mut push = |x: f64| {
        if x > 1e-9 && out.last().is_none_or(|l| (x - l).abs() > 1e-9) {
            out.push(x);
        }
    };
    for i in 0..pts.len() {
        if f[i].abs() <= eps {
            push(pts[i]);
        }
        if let Some(&fb) = f.get(i + 1)
            && f[i].abs() > eps
            && fb.abs() > eps
            && f[i] * fb < 0.0
        {
            push(pts[i] + (pts[i + 1] - pts[i]) * f[i] / (f[i] - fb));
        }
    }
    let m = tail_slope(legs);
    let (last, fl) = (pts[pts.len() - 1], f[f.len() - 1]);
    if m.abs() > 1e-9 && fl.abs() > eps && fl * m < 0.0 {
        push(last - fl / m);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    LongCall,
    CoveredCall,
    BullCallSpread,
    Straddle,
    Strangle,
    IronCondor,
    ProtectivePut,
}

impl Preset {
    pub const ALL: [Preset; 7] = [
        Preset::LongCall,
        Preset::CoveredCall,
        Preset::BullCallSpread,
        Preset::Straddle,
        Preset::Strangle,
        Preset::IronCondor,
        Preset::ProtectivePut,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::LongCall => "Long call",
            Preset::CoveredCall => "Covered call",
            Preset::BullCallSpread => "Vertical (bull call)",
            Preset::Straddle => "Straddle",
            Preset::Strangle => "Strangle",
            Preset::IronCondor => "Iron condor",
            Preset::ProtectivePut => "Protective put",
        }
    }
}

/// The listed strike nearest `target`.
fn near(strikes: &[f64], target: f64) -> Option<f64> {
    strikes.iter().copied().min_by(|a, b| (a - target).abs().total_cmp(&(b - target).abs()))
}

/// Legs of a preset as (kind, strike, qty), picking listed strikes (ascending) near the
/// money (100%) and wings at 95% / 105% (condor outer wings 90% / 110%). The stock leg's
/// "strike" is the spot. `None` when the strikes cannot form the shape (too few or colliding).
pub fn preset_legs(p: Preset, strikes: &[f64], spot: f64) -> Option<Vec<(LegKind, f64, i32)>> {
    use LegKind::{Call, Put, Stock};
    let at = |m: f64| near(strikes, spot * m);
    let legs = match p {
        Preset::LongCall => vec![(Call, at(1.0)?, 1)],
        Preset::CoveredCall => vec![(Stock, spot, 1), (Call, at(1.05)?, -1)],
        Preset::BullCallSpread => {
            let (lo, hi) = (at(1.0)?, at(1.05)?);
            if hi <= lo {
                return None;
            }
            vec![(Call, lo, 1), (Call, hi, -1)]
        }
        Preset::Straddle => {
            let k = at(1.0)?;
            vec![(Call, k, 1), (Put, k, 1)]
        }
        Preset::Strangle => {
            let (pk, ck) = (at(0.95)?, at(1.05)?);
            if pk >= ck {
                return None;
            }
            vec![(Put, pk, 1), (Call, ck, 1)]
        }
        Preset::IronCondor => {
            let (p1, p2, c1, c2) = (at(0.90)?, at(0.95)?, at(1.05)?, at(1.10)?);
            if !(p1 < p2 && p2 < c1 && c1 < c2) {
                return None;
            }
            vec![(Put, p1, 1), (Put, p2, -1), (Call, c1, -1), (Call, c2, 1)]
        }
        Preset::ProtectivePut => vec![(Stock, spot, 1), (Put, at(0.95)?, 1)],
    };
    Some(legs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn q(strike: f64, iv: Option<f64>, delta: Option<f64>, vol: Option<u64>, oi: Option<u64>) -> Quote {
        Quote { strike, iv, delta, volume: vol, open_interest: oi }
    }

    fn oi(strike: f64, oi: u64) -> Quote {
        q(strike, None, None, None, Some(oi))
    }

    fn leg(kind: LegKind, strike: f64, qty: i32, premium: f64) -> Leg {
        Leg { kind, strike, expiry: 1_800_000_000, qty, premium, iv: Some(0.2) }
    }

    #[test]
    fn atm_iv_averages_nearest_strikes_and_skips_missing_iv() {
        let calls = [q(95.0, Some(0.30), None, None, None), q(100.0, Some(0.20), None, None, None)];
        let puts = [q(100.0, None, None, None, None), q(95.0, Some(0.28), None, None, None)];
        // Call: 100 (0.20). Put: nearest with an IV is 95 (0.28).
        assert!(close(atm_iv(&calls, &puts, 101.0).unwrap(), 0.24));
        assert!(close(atm_iv(&calls, &[], 101.0).unwrap(), 0.20));
        assert_eq!(atm_iv(&[], &[], 100.0), None);
        assert_eq!(atm_iv(&[q(100.0, None, None, None, None)], &[], 100.0), None);
    }

    #[test]
    fn skew_picks_wings_nearest_25_delta() {
        let puts = [
            q(90.0, Some(0.34), Some(-0.10), None, None),
            q(94.0, Some(0.31), Some(-0.24), None, None),
            q(96.0, Some(0.29), Some(-0.33), None, None),
        ];
        let calls = [q(104.0, Some(0.25), Some(0.28), None, None), q(108.0, Some(0.24), Some(0.14), None, None)];
        let s = skew(&calls, &puts, 100.0).unwrap();
        assert_eq!(s.basis, SkewBasis::Delta);
        assert!(close(s.put_strike, 94.0) && close(s.call_strike, 104.0));
        assert!(close(s.skew, 0.06));
    }

    #[test]
    fn skew_falls_back_to_moneyness_without_deltas() {
        let puts = [q(95.0, Some(0.30), None, None, None), q(90.0, Some(0.35), None, None, None)];
        let calls = [q(105.0, Some(0.24), None, None, None), q(110.0, Some(0.25), None, None, None)];
        let s = skew(&calls, &puts, 100.0).unwrap();
        assert_eq!(s.basis, SkewBasis::Moneyness);
        assert!(close(s.put_strike, 95.0) && close(s.call_strike, 105.0));
        assert!(close(s.skew, 0.06));
        // Wings further than 3% of spot from 95/105 are not used.
        let far = [q(80.0, Some(0.4), None, None, None)];
        assert_eq!(skew(&calls, &far, 100.0), None);
        assert_eq!(skew(&[], &[], 100.0), None);
    }

    #[test]
    fn delta_outside_the_wing_range_is_ignored() {
        // Deltas 0.9 / -0.9 are deep ITM: fall back to moneyness (which also has no strikes).
        let puts = [q(150.0, Some(0.2), Some(-0.9), None, None)];
        let calls = [q(50.0, Some(0.2), Some(0.9), None, None)];
        assert_eq!(skew(&calls, &puts, 100.0), None);
    }

    #[test]
    fn put_call_ratios() {
        let calls = [q(100.0, None, None, Some(300), Some(1000)), q(105.0, None, None, Some(100), Some(500))];
        let puts = [q(95.0, None, None, Some(600), Some(1200)), q(100.0, None, None, None, Some(300))];
        let pc = PutCall::of(&calls, &puts);
        assert_eq!((pc.call_volume, pc.put_volume), (Some(400), Some(600)));
        assert!(close(pc.volume_ratio().unwrap(), 1.5));
        assert!(close(pc.oi_ratio().unwrap(), 1500.0 / 1500.0));
        let both = pc.plus(pc);
        assert_eq!(both.call_oi, Some(3000));
        assert!(close(both.volume_ratio().unwrap(), 1.5));
    }

    #[test]
    fn put_call_is_na_when_data_is_missing() {
        let calls = [q(100.0, None, None, None, None)];
        let puts = [q(100.0, None, None, Some(10), Some(5))];
        let pc = PutCall::of(&calls, &puts);
        assert_eq!(pc.call_volume, None);
        assert_eq!(pc.volume_ratio(), None);
        assert_eq!(pc.oi_ratio(), None);
        // Zero calls: no division by zero.
        let zero = PutCall::of(&[q(1.0, None, None, Some(0), Some(0))], &puts);
        assert_eq!(zero.volume_ratio(), None);
        assert_eq!(PutCall::default().plus(pc).put_volume, Some(10));
    }

    #[test]
    fn put_call_reading() {
        assert_eq!(interpret_put_call(0.5), "call-heavy: bullish positioning");
        assert_eq!(interpret_put_call(0.7), "balanced");
        assert_eq!(interpret_put_call(1.0), "balanced");
        assert!(interpret_put_call(1.2).starts_with("put-heavy"));
    }

    #[test]
    fn max_pain_hand_computed() {
        // Calls 100:10, 110:5. Puts 90:10, 100:5. Pain (contracts × $ ITM, × 100 shares):
        // S=90: puts 100 → 5×10 = 50. S=100: 0. S=110: calls 100 → 10×10 = 100.
        let calls = [oi(100.0, 10), oi(110.0, 5)];
        let puts = [oi(90.0, 10), oi(100.0, 5)];
        let mp = max_pain(&calls, &puts).unwrap();
        assert_eq!(mp.curve.len(), 3);
        assert!(close(mp.curve[0].1, 5_000.0) && close(mp.curve[1].1, 0.0) && close(mp.curve[2].1, 10_000.0));
        assert!(close(mp.strike, 100.0) && close(mp.payout, 0.0));
    }

    #[test]
    fn max_pain_ties_take_lowest_strike_and_need_oi() {
        let calls = [oi(100.0, 10), oi(110.0, 10)];
        let puts = [oi(100.0, 10), oi(110.0, 10)];
        // S=100: puts 110 → 10×10 = 100. S=110: calls 100 → 10×10 = 100. Tie → 100.
        let mp = max_pain(&calls, &puts).unwrap();
        assert!(close(mp.strike, 100.0) && close(mp.payout, 10_000.0));
        assert_eq!(max_pain(&[q(100.0, None, None, Some(5), None)], &[]), None);
        assert_eq!(max_pain(&[oi(100.0, 0)], &[]), None);
    }

    #[test]
    fn open_interest_walls() {
        let side = [oi(100.0, 50), oi(105.0, 80), oi(110.0, 50), q(115.0, None, None, None, None), oi(120.0, 0)];
        assert_eq!(top_oi(&side, 2), vec![(105.0, 80), (100.0, 50)]);
        assert_eq!(top_oi(&side, 10).len(), 3);
    }

    #[test]
    fn long_call_payoff_breakeven_and_bounds() {
        // Buy 1 call K=100 for 3.00: pay $300.
        let legs = [leg(LegKind::Call, 100.0, 1, 3.0)];
        assert!(close(net_premium(&legs), 300.0));
        assert!(close(pnl_at_expiry(&legs, 90.0), -300.0));
        assert!(close(pnl_at_expiry(&legs, 110.0), 700.0));
        assert_eq!(max_profit(&legs), Bound::Unlimited);
        assert_eq!(max_loss(&legs), Bound::Finite(-300.0));
        let be = breakevens(&legs);
        assert_eq!(be.len(), 1);
        assert!(close(be[0], 103.0));
    }

    #[test]
    fn short_call_has_unlimited_loss_and_credit() {
        let legs = [leg(LegKind::Call, 100.0, -2, 3.0)];
        assert!(close(net_premium(&legs), -600.0));
        assert_eq!(max_loss(&legs), Bound::Unlimited);
        assert_eq!(max_profit(&legs), Bound::Finite(600.0));
        assert!(close(breakevens(&legs)[0], 103.0));
    }

    #[test]
    fn straddle_has_two_breakevens() {
        // Long 100 call at 4 and 100 put at 3: debit 7 → break-evens 93 and 107.
        let legs = [leg(LegKind::Call, 100.0, 1, 4.0), leg(LegKind::Put, 100.0, 1, 3.0)];
        assert!(close(net_premium(&legs), 700.0));
        let be = breakevens(&legs);
        assert_eq!(be.len(), 2);
        assert!(close(be[0], 93.0) && close(be[1], 107.0));
        assert_eq!(max_profit(&legs), Bound::Unlimited);
        assert_eq!(max_loss(&legs), Bound::Finite(-700.0));
        // Put side is capped at the underlying reaching 0: not "unlimited" but large (K - 0 - 7)*100.
        assert!(close(pnl_at_expiry(&legs, 0.0), 9_300.0));
    }

    #[test]
    fn iron_condor_is_bounded_both_ways() {
        // Long 90P @0.5, short 95P @1.5, short 105C @1.5, long 110C @0.5: credit 2.0 → $200.
        let legs = [
            leg(LegKind::Put, 90.0, 1, 0.5),
            leg(LegKind::Put, 95.0, -1, 1.5),
            leg(LegKind::Call, 105.0, -1, 1.5),
            leg(LegKind::Call, 110.0, 1, 0.5),
        ];
        assert!(close(net_premium(&legs), -200.0));
        assert_eq!(max_profit(&legs), Bound::Finite(200.0));
        assert_eq!(max_loss(&legs), Bound::Finite(-300.0));
        let be = breakevens(&legs);
        assert_eq!(be.len(), 2);
        assert!(close(be[0], 93.0) && close(be[1], 107.0));
        assert!(close(pnl_at_expiry(&legs, 100.0), 200.0));
        assert!(close(pnl_at_expiry(&legs, 80.0), -300.0));
    }

    #[test]
    fn covered_call_and_protective_put() {
        // Stock bought at 100, short 105 call @2: profit capped at (5+2)*100 = 700.
        let cc = [leg(LegKind::Stock, 100.0, 1, 100.0), leg(LegKind::Call, 105.0, -1, 2.0)];
        assert_eq!(max_profit(&cc), Bound::Finite(700.0));
        assert!(close(breakevens(&cc)[0], 98.0));
        assert!(close(net_premium(&cc), 9_800.0));
        // Stock at 100 with long 95 put @2: loss floored at (5+2)*100 = 700, profit unlimited.
        let pp = [leg(LegKind::Stock, 100.0, 1, 100.0), leg(LegKind::Put, 95.0, 1, 2.0)];
        assert_eq!(max_loss(&pp), Bound::Finite(-700.0));
        assert_eq!(max_profit(&pp), Bound::Unlimited);
        assert!(close(breakevens(&pp)[0], 102.0));
    }

    #[test]
    fn breakeven_exactly_on_a_strike_and_empty_legs() {
        // Bull call spread 100/110 for a 4.0 debit: break-even at 104 (interior segment).
        let legs = [leg(LegKind::Call, 100.0, 1, 7.0), leg(LegKind::Call, 110.0, -1, 3.0)];
        assert!(close(breakevens(&legs)[0], 104.0));
        // Debit 0 at K=100 for a single call spread leg: payoff zero at the strike itself.
        let flat = [leg(LegKind::Call, 100.0, 1, 0.0)];
        assert_eq!(breakevens(&flat), vec![100.0]);
        assert!(breakevens(&[]).is_empty());
    }

    #[test]
    fn single_expiry_detection() {
        let a = leg(LegKind::Call, 100.0, 1, 1.0);
        let b = Leg { expiry: a.expiry + 86_400, ..a };
        let stock = leg(LegKind::Stock, 100.0, 1, 100.0);
        assert_eq!(single_expiry(&[a, stock]), Some(a.expiry));
        assert_eq!(single_expiry(&[a, b]), None);
        assert_eq!(single_expiry(&[stock]), None);
    }

    #[test]
    fn today_value_matches_black_scholes_and_converges_to_expiry() {
        // Hull: S=100 K=100 T=1 r=5% σ=20% call = 10.4505835722 (see options.rs tests).
        let now = 1_700_000_000;
        let expiry = now + 365 * 86_400 - 20 * 3600;
        let l = Leg { kind: LegKind::Call, strike: 100.0, expiry, qty: 1, premium: 10.0, iv: Some(0.2) };
        let v = pnl_today(&[l], 100.0, now, 0.05, 0.0).unwrap();
        assert!((v - (10.450_583_572_185_565 - 10.0) * 100.0).abs() < 1e-6, "{v}");
        // Expired: intrinsic, no IV needed.
        let past = Leg { expiry: now - 5 * 86_400, iv: None, ..l };
        assert!(close(pnl_today(&[past], 112.0, now, 0.05, 0.0).unwrap(), (12.0 - 10.0) * 100.0));
        // Live option without IV: n/a rather than a made-up number.
        let no_iv = Leg { iv: None, ..l };
        assert_eq!(pnl_today(&[no_iv], 100.0, now, 0.05, 0.0), None);
        assert!(position_greeks(&[no_iv], 100.0, now, 0.05, 0.0).is_none());
    }

    #[test]
    fn position_greeks_scale_and_net() {
        let now = 1_700_000_000;
        let expiry = now + 365 * 86_400 - 20 * 3600;
        let call = Leg { kind: LegKind::Call, strike: 100.0, expiry, qty: 2, premium: 10.0, iv: Some(0.2) };
        let g = position_greeks(&[call], 100.0, now, 0.05, 0.0).unwrap();
        // Hull delta 0.636830651175619 × 2 contracts × 100 shares.
        assert!((g.delta - 127.366_130_235_123_8).abs() < 1e-6, "{}", g.delta);
        assert!((g.vega - 0.375_240_346_916_938 * 200.0).abs() < 1e-6);
        // A short put at the same strike + long call = synthetic long: delta ≈ 2 × 100 × (0.6368 + 0.3632).
        let put = Leg { kind: LegKind::Put, qty: -2, ..call };
        let syn = position_greeks(&[call, put], 100.0, now, 0.05, 0.0).unwrap();
        assert!((syn.delta - 200.0).abs() < 1e-6);
        assert!(syn.gamma.abs() < 1e-9, "call and put gamma cancel when one is short");
        let stock = Leg { kind: LegKind::Stock, qty: 1, ..call };
        assert!(close(position_greeks(&[stock], 100.0, now, 0.05, 0.0).unwrap().delta, 100.0));
    }

    #[test]
    fn presets_pick_listed_strikes() {
        let strikes: Vec<f64> = (16..=24).map(|k| k as f64 * 5.0).collect(); // 80..=120
        let ks = |p| preset_legs(p, &strikes, 100.0).unwrap();
        assert_eq!(ks(Preset::LongCall), vec![(LegKind::Call, 100.0, 1)]);
        assert_eq!(ks(Preset::CoveredCall), vec![(LegKind::Stock, 100.0, 1), (LegKind::Call, 105.0, -1)]);
        assert_eq!(ks(Preset::BullCallSpread), vec![(LegKind::Call, 100.0, 1), (LegKind::Call, 105.0, -1)]);
        assert_eq!(ks(Preset::Straddle), vec![(LegKind::Call, 100.0, 1), (LegKind::Put, 100.0, 1)]);
        assert_eq!(ks(Preset::Strangle), vec![(LegKind::Put, 95.0, 1), (LegKind::Call, 105.0, 1)]);
        assert_eq!(
            ks(Preset::IronCondor),
            vec![
                (LegKind::Put, 90.0, 1),
                (LegKind::Put, 95.0, -1),
                (LegKind::Call, 105.0, -1),
                (LegKind::Call, 110.0, 1)
            ]
        );
        assert_eq!(ks(Preset::ProtectivePut), vec![(LegKind::Stock, 100.0, 1), (LegKind::Put, 95.0, 1)]);
        // Too coarse a ladder cannot form a condor; an empty one forms nothing.
        assert_eq!(preset_legs(Preset::IronCondor, &[100.0], 100.0), None);
        assert_eq!(preset_legs(Preset::LongCall, &[], 100.0), None);
    }
}
