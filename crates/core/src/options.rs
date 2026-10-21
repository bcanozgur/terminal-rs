//! Black-Scholes-Merton pricing and greeks for European options with a continuous
//! dividend yield. Used to derive greeks from a quoted implied volatility.

use serde::{Deserialize, Serialize};

/// Option sensitivities in trader units.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Greeks {
    /// dV/dS.
    pub delta: f64,
    /// d²V/dS².
    pub gamma: f64,
    /// Value change per calendar day.
    pub theta: f64,
    /// Value change per 1 volatility point (0.01).
    pub vega: f64,
    /// Value change per 1% rate move.
    pub rho: f64,
}

/// Standard normal CDF (Hart / West double-precision approximation, |error| < 1e-14).
pub fn norm_cdf(x: f64) -> f64 {
    let z = x.abs();
    let c = if z > 37.0 {
        0.0
    } else {
        let e = (-z * z / 2.0).exp();
        if z < 7.071_067_811_865_475 {
            const N: [f64; 7] = [
                0.035_262_496_599_891_1,
                0.700_383_064_443_688,
                6.373_962_203_531_65,
                33.912_866_078_383,
                112.079_291_497_871,
                221.213_596_169_931,
                220.206_867_912_376,
            ];
            const D: [f64; 8] = [
                0.088_388_347_648_318_4,
                1.755_667_163_182_64,
                16.064_177_579_207,
                86.780_732_202_946_1,
                296.564_248_779_674,
                637.333_633_378_831,
                793.826_512_519_948,
                440.413_735_824_752,
            ];
            let horner = |c: &[f64]| c.iter().fold(0.0, |acc, &k| acc * z + k);
            horner(&N) * e / horner(&D)
        } else {
            let mut f = z + 0.65;
            for k in [4.0, 3.0, 2.0, 1.0] {
                f = z + k / f;
            }
            e / f / 2.506_628_274_631_000_5
        }
    };
    if x > 0.0 { 1.0 - c } else { c }
}

pub fn norm_pdf(x: f64) -> f64 {
    (-x * x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Price and greeks. `t` is years to expiry, `r` and `q` continuous rates, `vol` annual.
/// Returns `None` when inputs cannot be priced (expired, zero vol, bad prices).
pub fn black_scholes(call: bool, spot: f64, strike: f64, t: f64, r: f64, q: f64, vol: f64) -> Option<(f64, Greeks)> {
    if !(spot > 0.0 && strike > 0.0 && t > 0.0 && vol > 0.0) || !(spot.is_finite() && vol.is_finite()) {
        return None;
    }
    let sq = vol * t.sqrt();
    let d1 = ((spot / strike).ln() + (r - q + vol * vol / 2.0) * t) / sq;
    let d2 = d1 - sq;
    let (dq, dr) = ((-q * t).exp(), (-r * t).exp());
    let pdf = norm_pdf(d1);
    let gamma = dq * pdf / (spot * sq);
    let vega = spot * dq * pdf * t.sqrt() / 100.0;
    let common = -spot * dq * pdf * vol / (2.0 * t.sqrt());
    let (price, delta, theta_y, rho) = if call {
        let (n1, n2) = (norm_cdf(d1), norm_cdf(d2));
        (
            spot * dq * n1 - strike * dr * n2,
            dq * n1,
            common - r * strike * dr * n2 + q * spot * dq * n1,
            strike * t * dr * n2 / 100.0,
        )
    } else {
        let (n1, n2) = (norm_cdf(-d1), norm_cdf(-d2));
        (
            strike * dr * n2 - spot * dq * n1,
            -dq * n1,
            common + r * strike * dr * n2 - q * spot * dq * n1,
            -strike * t * dr * n2 / 100.0,
        )
    };
    Some((price, Greeks { delta, gamma, theta: theta_y / 365.0, vega, rho }))
}

/// The volatility at which [`black_scholes`] gives `price`, by bisection over 1%–500%. `None`
/// when no volatility in that range gives it (a price under intrinsic value, or absurdly high).
pub fn implied_vol(call: bool, spot: f64, strike: f64, t: f64, r: f64, q: f64, price: f64) -> Option<f64> {
    let at = |v: f64| black_scholes(call, spot, strike, t, r, q, v).map(|(p, _)| p);
    let (mut lo, mut hi) = (0.01, 5.0);
    if !(price.is_finite() && price > 0.0) || at(lo)? > price || at(hi)? < price {
        return None;
    }
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if at(mid)? < price {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some((lo + hi) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn implied_vol_inverts_the_price() {
        for (call, k, v) in [(true, 100.0, 0.2), (false, 90.0, 0.35), (true, 120.0, 0.6)] {
            let (p, _) = black_scholes(call, 100.0, k, 0.25, 0.04, 0.01, v).unwrap();
            let iv = implied_vol(call, 100.0, k, 0.25, 0.04, 0.01, p).unwrap();
            assert!(close(iv, v, 1e-6), "{iv} vs {v}");
        }
        // Below intrinsic value: no volatility explains it.
        assert!(implied_vol(true, 100.0, 80.0, 0.25, 0.04, 0.0, 15.0).is_none());
        assert!(implied_vol(true, 100.0, 100.0, 0.25, 0.04, 0.0, 0.0).is_none());
    }

    #[test]
    fn normal_cdf_reference_values() {
        assert!(close(norm_cdf(0.0), 0.5, 1e-15));
        assert!(close(norm_cdf(1.0), 0.841_344_746_068_542_9, 1e-12));
        assert!(close(norm_cdf(-1.96), 0.024_997_895_148_220_4, 1e-12));
        assert!(close(norm_cdf(8.0), 1.0 - 6.220_960_574_271_78e-16, 1e-15));
        assert_eq!(norm_cdf(-40.0), 0.0);
    }

    #[test]
    fn textbook_call_and_put() {
        // Hull: S=100, K=100, T=1, r=5%, σ=20%, no dividends.
        let (c, g) = black_scholes(true, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2).unwrap();
        assert!(close(c, 10.450_583_572_185_565, 1e-9), "{c}");
        assert!(close(g.delta, 0.636_830_651_175_619, 1e-9));
        assert!(close(g.gamma, 0.018_762_017_345_846_9, 1e-9));
        assert!(close(g.vega, 0.375_240_346_916_938, 1e-9));
        assert!(close(g.theta * 365.0, -6.414_027_546_438_197, 1e-9));
        assert!(close(g.rho, 0.532_324_815_453_763, 1e-9));
        let (p, gp) = black_scholes(false, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2).unwrap();
        // Put-call parity: C - P = S - K e^{-rT}.
        assert!(close(c - p, 100.0 - 100.0 * (-0.05f64).exp(), 1e-9));
        assert!(close(gp.delta, g.delta - 1.0, 1e-12));
        assert!(close(gp.gamma, g.gamma, 1e-12));
        assert!(black_scholes(true, 100.0, 100.0, 0.0, 0.05, 0.0, 0.2).is_none());
        assert!(black_scholes(true, 100.0, 100.0, 1.0, 0.05, 0.0, 0.0).is_none());
    }

    #[test]
    fn dividend_yield_lowers_call_delta() {
        let (_, a) = black_scholes(true, 100.0, 100.0, 0.5, 0.04, 0.0, 0.3).unwrap();
        let (_, b) = black_scholes(true, 100.0, 100.0, 0.5, 0.04, 0.03, 0.3).unwrap();
        assert!(b.delta < a.delta);
    }
}
