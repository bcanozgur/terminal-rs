//! Portfolio risk maths on daily series: returns, historical and parametric VaR, expected
//! shortfall, beta/alpha/R², Sharpe/Sortino, drawdowns, correlation and risk contributions.
//!
//! Everything here is pure (slices in, numbers out). Returns are simple daily returns
//! (`0.01` = +1 %); losses (VaR, CVaR, drawdown depth) are reported as positive fractions
//! except [`drawdowns`], which is ≤ 0. Annualisation uses [`TRADING_DAYS`].

/// Trading days per year used to annualise.
pub const TRADING_DAYS: f64 = 252.0;

// ---- series ----------------------------------------------------------------------------

/// Day number (days since the Unix epoch, UTC) of a bar timestamp in seconds.
pub fn day_of(ts: i64) -> i64 {
    ts.div_euclid(86_400)
}

/// Inner-join several `(timestamp, close)` series on calendar day. Returns the shared days
/// (ascending) and, per input series, its closes on those days. Non-finite or non-positive
/// closes are ignored; the last close wins when a day repeats.
pub fn align_closes(series: &[Vec<(i64, f64)>]) -> (Vec<i64>, Vec<Vec<f64>>) {
    use std::collections::BTreeMap;
    let maps: Vec<BTreeMap<i64, f64>> = series
        .iter()
        .map(|s| s.iter().filter(|(_, c)| c.is_finite() && *c > 0.0).map(|&(t, c)| (day_of(t), c)).collect())
        .collect();
    let Some(first) = maps.first() else { return (Vec::new(), Vec::new()) };
    let days: Vec<i64> = first.keys().copied().filter(|d| maps.iter().all(|m| m.contains_key(d))).collect();
    let closes = maps.iter().map(|m| days.iter().map(|d| m[d]).collect()).collect();
    (days, closes)
}

/// Simple returns between consecutive closes.
pub fn returns(closes: &[f64]) -> Vec<f64> {
    closes.windows(2).map(|w| w[1] / w[0] - 1.0).collect()
}

/// Daily returns of a constant-weight portfolio (rebalanced every day). `asset_returns[i]`
/// is asset `i`'s series; all must be equally long.
pub fn portfolio_returns(asset_returns: &[Vec<f64>], weights: &[f64]) -> Vec<f64> {
    let n = asset_returns.first().map_or(0, Vec::len);
    (0..n).map(|t| asset_returns.iter().zip(weights).map(|(r, w)| r[t] * w).sum()).collect()
}

/// Cumulative value of 1 invested at the start: `curve[0] == 1`, one point per return plus
/// the starting point.
pub fn equity_curve(rets: &[f64]) -> Vec<f64> {
    let mut v = 1.0;
    let mut out = Vec::with_capacity(rets.len() + 1);
    out.push(v);
    for r in rets {
        v *= 1.0 + r;
        out.push(v);
    }
    out
}

// ---- moments ---------------------------------------------------------------------------

pub fn mean(x: &[f64]) -> f64 {
    if x.is_empty() { 0.0 } else { x.iter().sum::<f64>() / x.len() as f64 }
}

/// Sample covariance (n − 1). Zero for fewer than two points or unequal lengths.
pub fn cov(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len().min(y.len());
    if n < 2 {
        return 0.0;
    }
    let (mx, my) = (mean(&x[..n]), mean(&y[..n]));
    x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum::<f64>() / (n - 1) as f64
}

/// Sample standard deviation (n − 1).
pub fn stdev(x: &[f64]) -> f64 {
    cov(x, x).sqrt()
}

/// Pearson correlation; `None` when either series is constant or too short.
pub fn corr(x: &[f64], y: &[f64]) -> Option<f64> {
    let d = stdev(x) * stdev(y);
    (d > 1e-15).then(|| (cov(x, y) / d).clamp(-1.0, 1.0))
}

/// Annualised volatility of daily returns.
pub fn annual_vol(rets: &[f64]) -> f64 {
    stdev(rets) * TRADING_DAYS.sqrt()
}

/// Correlation matrix of equally long series; `None` where a series is constant.
pub fn corr_matrix(series: &[Vec<f64>]) -> Vec<Vec<Option<f64>>> {
    let n = series.len();
    let mut m = vec![vec![None; n]; n];
    for i in 0..n {
        for j in i..n {
            let c = if i == j { (stdev(&series[i]) > 1e-15).then_some(1.0) } else { corr(&series[i], &series[j]) };
            m[i][j] = c;
            m[j][i] = c;
        }
    }
    m
}

// ---- value at risk ---------------------------------------------------------------------

/// The `p`-quantile (0..1) of `x` with linear interpolation between order statistics.
pub fn quantile(x: &[f64], p: f64) -> Option<f64> {
    if x.is_empty() {
        return None;
    }
    let mut s: Vec<f64> = x.iter().copied().filter(|v| v.is_finite()).collect();
    s.sort_by(|a, b| a.total_cmp(b));
    if s.is_empty() {
        return None;
    }
    let h = (s.len() - 1) as f64 * p.clamp(0.0, 1.0);
    let (lo, hi) = (h.floor() as usize, h.ceil() as usize);
    Some(s[lo] + (s[hi] - s[lo]) * (h - lo as f64))
}

/// One-day historical VaR at confidence `conf` (0.95): the loss, as a positive fraction,
/// exceeded on `1 − conf` of days. Never negative.
pub fn var_historical(rets: &[f64], conf: f64) -> Option<f64> {
    quantile(rets, 1.0 - conf).map(|q| (-q).max(0.0))
}

/// Expected shortfall (CVaR): the average loss on the days at or beyond the VaR quantile.
pub fn cvar_historical(rets: &[f64], conf: f64) -> Option<f64> {
    let q = quantile(rets, 1.0 - conf)?;
    let tail: Vec<f64> = rets.iter().copied().filter(|r| r.is_finite() && *r <= q).collect();
    (!tail.is_empty()).then(|| (-mean(&tail)).max(0.0))
}

/// One-day parametric (normal) VaR from the sample mean and standard deviation.
pub fn var_parametric(rets: &[f64], conf: f64) -> Option<f64> {
    if rets.len() < 2 {
        return None;
    }
    Some((-(mean(rets) + norm_inv(1.0 - conf) * stdev(rets))).max(0.0))
}

/// Inverse of the standard normal CDF (Acklam's rational approximation, |error| < 1.2e-9).
#[allow(clippy::excessive_precision)]
pub fn norm_inv(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969683028665376e1,
        2.209460984245205e2,
        -2.759285104469687e2,
        1.383577518672690e2,
        -3.066479806614716e1,
        2.506628277459239,
    ];
    const B: [f64; 5] =
        [-5.447609879822406e1, 1.615858368580409e2, -1.556989798598866e2, 6.680131188771972e1, -1.328068155288572e1];
    const C: [f64; 6] = [
        -7.784894002430293e-3,
        -3.223964580411365e-1,
        -2.400758277161838,
        -2.549732539343734,
        4.374664141464968,
        2.938163982698783,
    ];
    const D: [f64; 4] = [7.784695709041462e-3, 3.224671290700398e-1, 2.445134137142996, 3.754408661907416];
    const LOW: f64 = 0.02425;
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    let tail = |q: f64| {
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    if p < LOW {
        tail((-2.0 * p.ln()).sqrt())
    } else if p > 1.0 - LOW {
        -tail((-2.0 * (1.0 - p).ln()).sqrt())
    } else {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    }
}

// ---- versus a benchmark ----------------------------------------------------------------

/// Regression of portfolio returns on benchmark returns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VsBenchmark {
    pub beta: f64,
    /// Jensen's alpha, annualised, with the risk-free rate taken out of both legs.
    pub alpha: f64,
    pub r_squared: f64,
    pub correlation: f64,
    /// Annualised standard deviation of the active return (portfolio − benchmark).
    pub tracking_error: f64,
    /// Annualised mean active return over the tracking error.
    pub information_ratio: Option<f64>,
}

/// Beta, alpha, R², correlation and tracking of `p` against `b`. `rf` is the annual
/// risk-free rate. `None` for fewer than 3 pairs or a constant benchmark.
pub fn vs_benchmark(p: &[f64], b: &[f64], rf: f64) -> Option<VsBenchmark> {
    let n = p.len().min(b.len());
    if n < 3 {
        return None;
    }
    let (p, b) = (&p[..n], &b[..n]);
    let vb = cov(b, b);
    if vb < 1e-18 {
        return None;
    }
    let beta = cov(p, b) / vb;
    let rf_d = rf / TRADING_DAYS;
    let alpha = (mean(p) - rf_d - beta * (mean(b) - rf_d)) * TRADING_DAYS;
    let correlation = corr(p, b).unwrap_or(0.0);
    let active: Vec<f64> = p.iter().zip(b).map(|(a, c)| a - c).collect();
    let te = stdev(&active) * TRADING_DAYS.sqrt();
    Some(VsBenchmark {
        beta,
        alpha,
        r_squared: correlation * correlation,
        correlation,
        tracking_error: te,
        information_ratio: (te > 1e-12).then(|| mean(&active) * TRADING_DAYS / te),
    })
}

/// Annualised Sharpe ratio: mean excess return over its standard deviation.
pub fn sharpe(rets: &[f64], rf: f64) -> Option<f64> {
    let sd = stdev(rets);
    (sd > 1e-15).then(|| (mean(rets) - rf / TRADING_DAYS) / sd * TRADING_DAYS.sqrt())
}

/// Annualised Sortino ratio: excess return over the downside deviation below the
/// risk-free rate.
pub fn sortino(rets: &[f64], rf: f64) -> Option<f64> {
    if rets.len() < 2 {
        return None;
    }
    let mar = rf / TRADING_DAYS;
    let dd = (rets.iter().map(|r| (r - mar).min(0.0).powi(2)).sum::<f64>() / rets.len() as f64).sqrt();
    (dd > 1e-15).then(|| (mean(rets) - mar) / dd * TRADING_DAYS.sqrt())
}

// ---- drawdown --------------------------------------------------------------------------

/// Drawdown from the running peak at every point of an equity curve (≤ 0).
pub fn drawdowns(equity: &[f64]) -> Vec<f64> {
    let mut peak = f64::MIN;
    equity
        .iter()
        .map(|&v| {
            peak = peak.max(v);
            if peak > 0.0 { v / peak - 1.0 } else { 0.0 }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawdownStats {
    /// Deepest drawdown as a positive fraction.
    pub max: f64,
    /// Index of the peak before, and the trough of, the deepest drawdown.
    pub max_peak: usize,
    pub max_trough: usize,
    /// Drawdown now, as a positive fraction (0 at a new high).
    pub current: f64,
}

pub fn drawdown_stats(equity: &[f64]) -> Option<DrawdownStats> {
    if equity.is_empty() {
        return None;
    }
    let dd = drawdowns(equity);
    let (mut peak_i, mut best) = (0usize, DrawdownStats { max: 0.0, max_peak: 0, max_trough: 0, current: 0.0 });
    let mut peak = equity[0];
    for (i, &v) in equity.iter().enumerate() {
        if v > peak {
            peak = v;
            peak_i = i;
        }
        let depth = -dd[i];
        if depth > best.max {
            best = DrawdownStats { max: depth, max_peak: peak_i, max_trough: i, current: 0.0 };
        }
    }
    best.current = -dd[dd.len() - 1];
    Some(best)
}

// ---- risk contribution -----------------------------------------------------------------

/// One asset's share of portfolio volatility.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contribution {
    /// Annualised stand-alone volatility.
    pub vol: f64,
    /// Beta to the benchmark, when one was given.
    pub beta: Option<f64>,
    /// ∂σp/∂wᵢ, annualised: the portfolio vol added per unit of weight.
    pub marginal: f64,
    /// wᵢ · marginal: the part of the annualised portfolio vol this asset accounts for.
    pub component: f64,
    /// `component / σp`; these sum to 1.
    pub share: f64,
}

/// Euler decomposition of portfolio volatility. `asset_returns[i]` pairs with `weights[i]`;
/// `bench` supplies the betas.
pub fn contributions(asset_returns: &[Vec<f64>], weights: &[f64], bench: Option<&[f64]>) -> Vec<Contribution> {
    let n = asset_returns.len();
    let cov_m: Vec<Vec<f64>> =
        (0..n).map(|i| (0..n).map(|j| cov(&asset_returns[i], &asset_returns[j]) * TRADING_DAYS).collect()).collect();
    let sigma_w: Vec<f64> = (0..n).map(|i| (0..n).map(|j| cov_m[i][j] * weights[j]).sum()).collect();
    let var: f64 = weights.iter().zip(&sigma_w).map(|(w, s)| w * s).sum();
    let sp = var.max(0.0).sqrt();
    (0..n)
        .map(|i| {
            let marginal = if sp > 1e-15 { sigma_w[i] / sp } else { 0.0 };
            let component = weights[i] * marginal;
            Contribution {
                vol: cov_m[i][i].max(0.0).sqrt(),
                beta: bench.and_then(|b| {
                    let vb = cov(b, b);
                    (vb > 1e-18).then(|| cov(&asset_returns[i], b) / vb)
                }),
                marginal,
                component,
                share: if sp > 1e-15 { component / sp } else { 0.0 },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    /// Deterministic pseudo-random series in about [-1, 1].
    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((s >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn returns_and_alignment() {
        let r = returns(&[100.0, 110.0, 99.0]);
        close(r[0], 0.1);
        close(r[1], -0.1);
        let a = vec![(0, 1.0), (86_400, 2.0), (2 * 86_400, 3.0)];
        let b = vec![(86_400 + 5, 20.0), (2 * 86_400 + 9, 30.0), (3 * 86_400, 40.0)];
        let (days, c) = align_closes(&[a, b]);
        assert_eq!(days, vec![1, 2]);
        assert_eq!(c, vec![vec![2.0, 3.0], vec![20.0, 30.0]]);
        assert!(align_closes(&[]).0.is_empty());
    }

    #[test]
    fn historical_var_and_cvar_on_known_losses() {
        // 100 returns: -1% .. -100% worst first is not realistic, but the quantiles are exact.
        let r: Vec<f64> = (1..=100).map(|i| i as f64 / 100.0 - 0.5).collect(); // -0.49 .. 0.50
        // 5% quantile with linear interpolation: h = 99 * 0.05 = 4.95 between -0.45 and -0.44.
        close(var_historical(&r, 0.95).unwrap(), 0.4405);
        // Tail: returns <= -0.4405 are the five worst: -0.49..-0.45, mean -0.47.
        close(cvar_historical(&r, 0.95).unwrap(), 0.47);
        assert!(cvar_historical(&r, 0.95).unwrap() >= var_historical(&r, 0.95).unwrap());
        // A rising series has no loss quantile: VaR floors at zero.
        assert_eq!(var_historical(&[0.01, 0.02, 0.03], 0.95), Some(0.0));
        assert_eq!(var_historical(&[], 0.95), None);
    }

    #[test]
    fn normal_quantiles() {
        assert!((norm_inv(0.975) - 1.959964).abs() < 1e-5);
        assert!((norm_inv(0.05) + 1.644854).abs() < 1e-5);
        assert!((norm_inv(0.01) + 2.326348).abs() < 1e-5);
        assert!(norm_inv(0.5).abs() < 1e-9);
    }

    #[test]
    fn parametric_var_matches_the_formula() {
        let r = noise(500, 7);
        let want = -(mean(&r) - 1.644854 * stdev(&r));
        assert!((var_parametric(&r, 0.95).unwrap() - want).abs() < 1e-4);
        // On roughly normal data the two estimates agree loosely; 99% is deeper than 95%.
        assert!(var_parametric(&r, 0.99).unwrap() > var_parametric(&r, 0.95).unwrap());
        assert_eq!(var_parametric(&[0.1], 0.95), None);
    }

    #[test]
    fn identical_series_have_beta_one_and_no_tracking() {
        let r = noise(300, 1);
        let v = vs_benchmark(&r, &r, 0.04).unwrap();
        close(v.beta, 1.0);
        close(v.alpha, 0.0);
        close(v.r_squared, 1.0);
        close(v.correlation, 1.0);
        close(v.tracking_error, 0.0);
        assert_eq!(v.information_ratio, None);
    }

    #[test]
    fn leveraged_series_has_beta_two() {
        let b = noise(300, 2);
        let p: Vec<f64> = b.iter().map(|x| x * 2.0).collect();
        let v = vs_benchmark(&p, &b, 0.0).unwrap();
        close(v.beta, 2.0);
        close(v.r_squared, 1.0);
        // Pure alpha: a constant daily bonus on top of the benchmark.
        let q: Vec<f64> = b.iter().map(|x| x + 0.001).collect();
        let v = vs_benchmark(&q, &b, 0.0).unwrap();
        close(v.beta, 1.0);
        close(v.alpha, 0.001 * TRADING_DAYS);
    }

    #[test]
    fn benchmark_edge_cases() {
        assert_eq!(vs_benchmark(&[0.1, 0.2], &[0.1, 0.2], 0.0), None);
        assert_eq!(vs_benchmark(&[0.1, 0.2, 0.3], &[0.0, 0.0, 0.0], 0.0), None);
        assert_eq!(corr(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]), None);
        close(corr(&[1.0, 2.0, 3.0], &[3.0, 2.0, 1.0]).unwrap(), -1.0);
    }

    #[test]
    fn sharpe_and_sortino() {
        let r = vec![0.01, -0.01, 0.02, -0.02, 0.01, 0.00];
        let m = mean(&r);
        let sd = stdev(&r);
        close(sharpe(&r, 0.0).unwrap(), m / sd * 252f64.sqrt());
        // Risk-free reduces the Sharpe ratio.
        assert!(sharpe(&r, 0.04).unwrap() < sharpe(&r, 0.0).unwrap());
        let dd = ((0.01f64.powi(2) + 0.02f64.powi(2)) / 6.0).sqrt();
        close(sortino(&r, 0.0).unwrap(), m / dd * 252f64.sqrt());
        assert_eq!(sharpe(&[0.01, 0.01, 0.01], 0.0), None);
        assert_eq!(sortino(&[0.01, 0.02], 0.0), None); // no downside
    }

    #[test]
    fn drawdown_on_a_hand_made_curve() {
        let eq = [1.0, 1.2, 0.9, 1.5, 1.0, 1.2];
        let dd = drawdowns(&eq);
        close(dd[0], 0.0);
        close(dd[2], 0.9 / 1.2 - 1.0);
        close(dd[3], 0.0);
        close(dd[4], 1.0 / 1.5 - 1.0);
        let s = drawdown_stats(&eq).unwrap();
        close(s.max, 1.0 - 1.0 / 1.5);
        assert_eq!((s.max_peak, s.max_trough), (3, 4));
        close(s.current, 1.0 - 1.2 / 1.5);
        // At a new high the current drawdown is zero.
        assert_eq!(drawdown_stats(&[1.0, 2.0]).unwrap().current, 0.0);
        assert_eq!(drawdown_stats(&[]), None);
        // Monotonic rise: no drawdown at all.
        assert_eq!(drawdown_stats(&[1.0, 1.1, 1.2]).unwrap().max, 0.0);
    }

    #[test]
    fn equity_curve_compounds() {
        let c = equity_curve(&[0.1, -0.1]);
        assert_eq!(c.len(), 3);
        close(c[2], 0.99);
    }

    #[test]
    fn portfolio_returns_weight_the_assets() {
        let a = vec![0.01, 0.02];
        let b = vec![-0.01, 0.04];
        let p = portfolio_returns(&[a, b], &[0.75, 0.25]);
        close(p[0], 0.005);
        close(p[1], 0.025);
    }

    #[test]
    fn correlation_matrix_is_symmetric_with_unit_diagonal() {
        let x = noise(100, 3);
        let y: Vec<f64> = x.iter().map(|v| -v).collect();
        let z = vec![0.0; 100];
        let m = corr_matrix(&[x, y, z]);
        close(m[0][0].unwrap(), 1.0);
        close(m[0][1].unwrap(), -1.0);
        assert_eq!(m[0][1], m[1][0]);
        assert_eq!(m[2][2], None);
        assert_eq!(m[0][2], None);
    }

    #[test]
    fn contributions_sum_to_the_portfolio_vol() {
        let a = noise(250, 4);
        let b = noise(250, 5);
        let w = [0.6, 0.4];
        let rets = [a, b];
        let port = portfolio_returns(&rets, &w);
        let c = contributions(&rets, &w, Some(&port));
        close(c.iter().map(|x| x.component).sum::<f64>(), annual_vol(&port));
        close(c.iter().map(|x| x.share).sum::<f64>(), 1.0);
        // The portfolio's own beta to itself is 1, so the weighted betas sum to 1.
        close(c.iter().zip(&w).map(|(x, w)| x.beta.unwrap() * w).sum::<f64>(), 1.0);
    }

    #[test]
    fn a_single_asset_carries_all_the_risk() {
        let a = noise(100, 6);
        let c = contributions(std::slice::from_ref(&a), &[1.0], None);
        close(c[0].share, 1.0);
        close(c[0].vol, annual_vol(&a));
        assert_eq!(c[0].beta, None);
        // Zero weights: no portfolio risk, no division blow-ups.
        let z = contributions(&[a], &[0.0], None);
        assert_eq!(z[0].share, 0.0);
    }
}
