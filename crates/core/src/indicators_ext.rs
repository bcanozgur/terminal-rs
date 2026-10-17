//! More technical indicators: ATR, Stochastic, ADX/DI, Ichimoku, OBV, anchored VWAP and the
//! volume profile. Same conventions as [`crate::indicators`]: every series is aligned with its
//! input and positions without enough history hold `f64::NAN` (Ichimoku's leading spans are
//! the exception: they run `kijun` bars past the last input).

use crate::Bar;

/// Smooth with an SMA, skipping the leading NaNs of `values` (a plain `sma` over a series
/// that starts with NaN would never recover).
fn sma_tail(values: &[f64], n: usize) -> Vec<f64> {
    let mut out = vec![f64::NAN; values.len()];
    let Some(first) = values.iter().position(|v| v.is_finite()) else { return out };
    if n == 0 || values.len() - first < n {
        return out;
    }
    let tail = crate::indicators::sma(&values[first..], n);
    out[first..].copy_from_slice(&tail);
    out
}

/// True range: the widest of high-low and the gaps to the previous close (the first bar is
/// just high-low).
pub fn true_range(bars: &[Bar]) -> Vec<f64> {
    bars.iter()
        .enumerate()
        .map(|(i, b)| {
            let hl = b.high - b.low;
            if i == 0 {
                hl
            } else {
                let pc = bars[i - 1].close;
                hl.max((b.high - pc).abs()).max((b.low - pc).abs())
            }
        })
        .collect()
}

/// Average true range with Wilder smoothing, seeded with the mean of the first `n` true
/// ranges (first value at index `n - 1`).
pub fn atr(bars: &[Bar], n: usize) -> Vec<f64> {
    crate::indicators::rma(&true_range(bars), n)
}

/// Stochastic oscillator: raw %K over `k` bars, smoothed with an SMA of `smooth` (1 = fast
/// stochastic), and %D = SMA(`d`) of the smoothed %K.
pub fn stochastic(bars: &[Bar], k: usize, smooth: usize, d: usize) -> (Vec<f64>, Vec<f64>) {
    let mut raw = vec![f64::NAN; bars.len()];
    if k > 0 && bars.len() >= k {
        for i in (k - 1)..bars.len() {
            let w = &bars[i + 1 - k..=i];
            let hh = w.iter().map(|b| b.high).fold(f64::MIN, f64::max);
            let ll = w.iter().map(|b| b.low).fold(f64::MAX, f64::min);
            // A flat window has no range: call it the middle rather than divide by zero.
            raw[i] = if hh > ll { 100.0 * (bars[i].close - ll) / (hh - ll) } else { 50.0 };
        }
    }
    let line = sma_tail(&raw, smooth);
    let signal = sma_tail(&line, d);
    (line, signal)
}

#[derive(Debug, Clone)]
pub struct Adx {
    pub plus_di: Vec<f64>,
    pub minus_di: Vec<f64>,
    pub adx: Vec<f64>,
}

#[allow(clippy::needless_range_loop)] // the loops index several parallel series
/// Wilder's directional movement system. +DI / -DI start at index `n`, ADX at `2n - 1`.
pub fn adx(bars: &[Bar], n: usize) -> Adx {
    let len = bars.len();
    let mut out = Adx { plus_di: vec![f64::NAN; len], minus_di: vec![f64::NAN; len], adx: vec![f64::NAN; len] };
    if n == 0 || len <= n {
        return out;
    }
    let tr = true_range(bars);
    let dm = |i: usize| {
        let up = bars[i].high - bars[i - 1].high;
        let down = bars[i - 1].low - bars[i].low;
        let plus = if up > down && up > 0.0 { up } else { 0.0 };
        let minus = if down > up && down > 0.0 { down } else { 0.0 };
        (plus, minus)
    };
    let nf = n as f64;
    // Wilder sums over the first n moves, then sm = sm - sm / n + current.
    let (mut str_, mut sp, mut sm) = (0.0, 0.0, 0.0);
    for i in 1..=n {
        let (p, m) = dm(i);
        str_ += tr[i];
        sp += p;
        sm += m;
    }
    let mut dx = vec![f64::NAN; len];
    let mut record = |i: usize, str_: f64, sp: f64, sm: f64, out: &mut Adx| {
        let (pdi, mdi) = if str_ > 0.0 { (100.0 * sp / str_, 100.0 * sm / str_) } else { (0.0, 0.0) };
        out.plus_di[i] = pdi;
        out.minus_di[i] = mdi;
        dx[i] = if pdi + mdi > 0.0 { 100.0 * (pdi - mdi).abs() / (pdi + mdi) } else { 0.0 };
    };
    record(n, str_, sp, sm, &mut out);
    for i in (n + 1)..len {
        let (p, m) = dm(i);
        str_ = str_ - str_ / nf + tr[i];
        sp = sp - sp / nf + p;
        sm = sm - sm / nf + m;
        record(i, str_, sp, sm, &mut out);
    }
    let first = 2 * n - 1;
    if len > first {
        let mut a = dx[n..=first].iter().sum::<f64>() / nf;
        out.adx[first] = a;
        for i in (first + 1)..len {
            a = (a * (nf - 1.0) + dx[i]) / nf;
            out.adx[i] = a;
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct Ichimoku {
    pub tenkan: Vec<f64>,
    pub kijun: Vec<f64>,
    /// Leading span A, shifted `kijun` bars forward: length `bars + kijun`.
    pub senkou_a: Vec<f64>,
    /// Leading span B, shifted like A.
    pub senkou_b: Vec<f64>,
    /// The close shifted `kijun` bars back (aligned with the input; the last `kijun` are NaN).
    pub chikou: Vec<f64>,
}

/// Midpoint of the highest high and lowest low over `n` bars ending at each index.
fn mid_range(bars: &[Bar], n: usize) -> Vec<f64> {
    let mut out = vec![f64::NAN; bars.len()];
    if n == 0 || bars.len() < n {
        return out;
    }
    for i in (n - 1)..bars.len() {
        let w = &bars[i + 1 - n..=i];
        let hh = w.iter().map(|b| b.high).fold(f64::MIN, f64::max);
        let ll = w.iter().map(|b| b.low).fold(f64::MAX, f64::min);
        out[i] = (hh + ll) / 2.0;
    }
    out
}

/// Ichimoku Cloud with periods `(tenkan, kijun, senkou B)`, classically (9, 26, 52); the
/// displacement equals the kijun period.
pub fn ichimoku(bars: &[Bar], tenkan: usize, kijun: usize, senkou: usize) -> Ichimoku {
    let len = bars.len();
    let t = mid_range(bars, tenkan);
    let k = mid_range(bars, kijun);
    let b = mid_range(bars, senkou);
    let mut senkou_a = vec![f64::NAN; len + kijun];
    let mut senkou_b = vec![f64::NAN; len + kijun];
    for i in 0..len {
        senkou_a[i + kijun] = (t[i] + k[i]) / 2.0;
        senkou_b[i + kijun] = b[i];
    }
    let mut chikou = vec![f64::NAN; len];
    for i in kijun..len {
        chikou[i - kijun] = bars[i].close;
    }
    Ichimoku { tenkan: t, kijun: k, senkou_a, senkou_b, chikou }
}

/// On-balance volume: running total of volume signed by the close-to-close direction
/// (starts at 0).
pub fn obv(bars: &[Bar]) -> Vec<f64> {
    let mut acc = 0.0;
    bars.iter()
        .enumerate()
        .map(|(i, b)| {
            if i > 0 {
                let pc = bars[i - 1].close;
                if b.close > pc {
                    acc += b.volume;
                } else if b.close < pc {
                    acc -= b.volume;
                }
            }
            acc
        })
        .collect()
}

/// VWAP over hlc3 accumulated from bar `anchor` onwards (NaN before it).
pub fn anchored_vwap(bars: &[Bar], anchor: usize) -> Vec<f64> {
    let mut out = vec![f64::NAN; bars.len()];
    let (mut pv, mut vol) = (0.0, 0.0);
    for (i, b) in bars.iter().enumerate().skip(anchor) {
        pv += b.hlc3() * b.volume;
        vol += b.volume;
        out[i] = if vol > 0.0 { pv / vol } else { b.hlc3() };
    }
    out
}

/// Index of the first bar at or after `ts` (the bar an anchor timestamp refers to).
pub fn anchor_index(bars: &[Bar], ts: i64) -> Option<usize> {
    let i = bars.partition_point(|b| b.ts < ts);
    (i < bars.len()).then_some(i)
}

/// Volume by price over a stretch of bars.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeProfile {
    /// Price at the bottom of row 0.
    pub lo: f64,
    /// Price height of one row.
    pub step: f64,
    pub volumes: Vec<f64>,
    /// Row with the most volume (point of control).
    pub poc: usize,
    /// Inclusive row range holding the value area.
    pub va: (usize, usize),
}

impl VolumeProfile {
    pub fn row_bottom(&self, row: usize) -> f64 {
        self.lo + self.step * row as f64
    }

    pub fn poc_price(&self) -> f64 {
        self.row_bottom(self.poc) + self.step / 2.0
    }

    pub fn va_low(&self) -> f64 {
        self.row_bottom(self.va.0)
    }

    pub fn va_high(&self) -> f64 {
        self.row_bottom(self.va.1 + 1)
    }
}

/// Share of the total volume the value area covers.
pub const VALUE_AREA: f64 = 0.70;

/// Histogram of volume over `rows` equal price rows spanning the bars' low-to-high range.
/// A bar's volume is spread evenly across the rows its high-low range touches (a bar with no
/// range goes to one row). The value area grows from the point of control, each step taking
/// the larger neighbouring row, until it holds [`VALUE_AREA`] of the volume.
pub fn volume_profile(bars: &[Bar], rows: usize) -> Option<VolumeProfile> {
    if rows == 0 || bars.is_empty() {
        return None;
    }
    let lo = bars.iter().map(|b| b.low).fold(f64::MAX, f64::min);
    let hi = bars.iter().map(|b| b.high).fold(f64::MIN, f64::max);
    if !(lo.is_finite() && hi.is_finite()) || hi < lo {
        return None;
    }
    let step = ((hi - lo) / rows as f64).max(f64::EPSILON * hi.abs().max(1.0));
    let mut volumes = vec![0.0; rows];
    let row_of = |p: f64| (((p - lo) / step).floor().max(0.0) as usize).min(rows - 1);
    for b in bars.iter().filter(|b| b.volume > 0.0) {
        if b.high - b.low < step * 1e-9 {
            volumes[row_of(b.close)] += b.volume;
            continue;
        }
        let (r0, r1) = (row_of(b.low), row_of(b.high));
        let span = b.high - b.low;
        for (r, v) in volumes.iter_mut().enumerate().take(r1 + 1).skip(r0) {
            let (bottom, top) = (lo + step * r as f64, lo + step * (r + 1) as f64);
            let overlap = (top.min(b.high) - bottom.max(b.low)).max(0.0);
            *v += b.volume * overlap / span;
        }
    }
    let total: f64 = volumes.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let poc = volumes.iter().enumerate().fold(0, |best, (i, v)| if *v > volumes[best] { i } else { best });
    let (mut a, mut z) = (poc, poc);
    let mut held = volumes[poc];
    while held < VALUE_AREA * total - 1e-9 {
        let below = a.checked_sub(1).map(|i| volumes[i]);
        let above = (z + 1 < rows).then(|| volumes[z + 1]);
        match (below, above) {
            (Some(b), Some(u)) if b > u => {
                a -= 1;
                held += b;
            }
            (_, Some(u)) => {
                z += 1;
                held += u;
            }
            (Some(b), None) => {
                a -= 1;
                held += b;
            }
            (None, None) => break,
        }
    }
    Some(VolumeProfile { lo, step, volumes, poc, va: (a, z) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(h: f64, l: f64, c: f64) -> Bar {
        Bar::new(0, c, h, l, c, 1.0)
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn true_range_and_wilder_atr() {
        // TR = [2, 3, 4 (gap up from 11), 2 (gap down from 14)].
        let bars = [bar(10.0, 8.0, 9.0), bar(12.0, 9.0, 11.0), bar(15.0, 13.0, 14.0), bar(14.0, 12.0, 13.0)];
        assert_eq!(true_range(&bars), vec![2.0, 3.0, 4.0, 2.0]);
        let a = atr(&bars, 3);
        assert!(a[1].is_nan());
        assert!(near(a[2], 3.0)); // (2 + 3 + 4) / 3
        assert!(near(a[3], 8.0 / 3.0)); // (3 * 2 + 2) / 3
    }

    #[test]
    fn stochastic_k_and_d() {
        let bars = [bar(10.0, 6.0, 8.0), bar(12.0, 7.0, 11.0), bar(11.0, 5.0, 6.0), bar(13.0, 8.0, 12.0)];
        let (k, d) = stochastic(&bars, 3, 1, 2);
        assert!(k[1].is_nan());
        assert!(near(k[2], 100.0 / 7.0)); // (6 - 5) / (12 - 5)
        assert!(near(k[3], 87.5)); // (12 - 5) / (13 - 5)
        assert!(d[2].is_nan());
        assert!(near(d[3], (100.0 / 7.0 + 87.5) / 2.0));
        // Slow smoothing starts after the raw %K is defined, not at index 0.
        let (slow, _) = stochastic(&bars, 3, 2, 1);
        assert!(slow[2].is_nan() && near(slow[3], (100.0 / 7.0 + 87.5) / 2.0));
    }

    #[test]
    fn stochastic_flat_window_is_midpoint() {
        let bars = [bar(5.0, 5.0, 5.0); 4];
        assert_eq!(stochastic(&bars, 3, 1, 1).0[3], 50.0);
    }

    #[test]
    fn adx_wilder_reference() {
        let bars = [
            bar(10.0, 8.0, 9.0),
            bar(12.0, 9.0, 11.0),
            bar(13.0, 10.0, 12.0),
            bar(12.0, 8.0, 9.0),
            bar(11.0, 7.0, 8.0),
        ];
        let a = adx(&bars, 2);
        assert!(a.plus_di[1].is_nan());
        // idx 2: smoothed TR 6, +DM 3, -DM 0.
        assert!(near(a.plus_di[2], 50.0) && near(a.minus_di[2], 0.0));
        // idx 3: TR 7, +DM 1.5, -DM 2.
        assert!(near(a.plus_di[3], 100.0 * 1.5 / 7.0) && near(a.minus_di[3], 100.0 * 2.0 / 7.0));
        // idx 4: TR 7.5, +DM 0.75, -DM 2.
        assert!(near(a.plus_di[4], 10.0) && near(a.minus_di[4], 100.0 * 2.0 / 7.5));
        // DX = 100, 14.2857, 45.4545; ADX seeds with the mean of the first two, then smooths.
        assert!(a.adx[2].is_nan());
        assert!(near(a.adx[3], 57.142_857));
        assert!(near(a.adx[4], 51.298_701));
    }

    #[test]
    fn ichimoku_lines_and_shifts() {
        let hl = [(10.0, 8.0), (12.0, 9.0), (11.0, 7.0), (13.0, 10.0), (14.0, 11.0), (12.0, 9.0)];
        let bars: Vec<Bar> = hl.iter().enumerate().map(|(i, (h, l))| bar(*h, *l, 10.0 + i as f64)).collect();
        let ich = ichimoku(&bars, 2, 3, 3);
        assert!(near(ich.tenkan[1], 10.0) && near(ich.tenkan[2], 9.5));
        assert!(near(ich.kijun[2], 9.5) && near(ich.kijun[3], 10.0));
        assert_eq!(ich.senkou_a.len(), 9);
        assert!(ich.senkou_a[4].is_nan());
        assert!(near(ich.senkou_a[5], 9.5)); // (tenkan[2] + kijun[2]) / 2, shown 3 bars later
        assert!(near(ich.senkou_a[6], 10.0));
        assert!(near(ich.senkou_b[5], 9.5) && near(ich.senkou_b[6], 10.0));
        assert_eq!(ich.chikou[0], 13.0); // close of bar 3 plotted 3 bars back
        assert!(ich.chikou[3].is_nan());
    }

    #[test]
    fn obv_signs_volume() {
        let mk = |c: f64, v: f64| Bar::new(0, c, c, c, c, v);
        let bars = [mk(10.0, 100.0), mk(11.0, 200.0), mk(10.5, 150.0), mk(10.5, 300.0), mk(12.0, 50.0)];
        assert_eq!(obv(&bars), vec![0.0, 200.0, 50.0, 50.0, 100.0]);
    }

    #[test]
    fn anchored_vwap_accumulates_from_anchor() {
        let mk = |p: f64, v: f64| Bar::new(0, p, p, p, p, v);
        let bars = [mk(10.0, 100.0), mk(12.0, 100.0), mk(15.0, 200.0)];
        let v = anchored_vwap(&bars, 1);
        assert!(v[0].is_nan());
        assert!(near(v[1], 12.0));
        assert!(near(v[2], 14.0)); // (12 * 100 + 15 * 200) / 300
        assert_eq!(anchor_index(&bars, 0), Some(0));
        let mut late = bars;
        late[2].ts = 50;
        assert_eq!(anchor_index(&late, 10), Some(2));
        assert_eq!(anchor_index(&late, 60), None);
    }

    #[test]
    fn volume_profile_poc_and_value_area() {
        let a = Bar::new(0, 10.0, 12.0, 10.0, 11.0, 100.0);
        let b = Bar::new(1, 10.0, 14.0, 10.0, 12.0, 400.0);
        let p = volume_profile(&[a, b], 4).unwrap();
        assert_eq!((p.lo, p.step), (10.0, 1.0));
        assert!(p.volumes.iter().zip([150.0, 150.0, 100.0, 100.0]).all(|(x, y)| near(*x, y)));
        assert_eq!(p.poc, 0);
        // 70% of 500 = 350: rows 0 (150) + 1 (150) + 2 (100).
        assert_eq!(p.va, (0, 2));
        assert!(near(p.poc_price(), 10.5) && near(p.va_low(), 10.0) && near(p.va_high(), 13.0));
        assert!(volume_profile(&[], 4).is_none());
    }

    #[test]
    fn volume_profile_flat_bar_goes_to_one_row() {
        let a = Bar::new(0, 10.0, 10.0, 10.0, 10.0, 5.0);
        let b = Bar::new(1, 20.0, 20.0, 20.0, 20.0, 1.0);
        let p = volume_profile(&[a, b], 5).unwrap();
        assert_eq!(p.volumes[0], 5.0);
        assert_eq!(p.volumes[4], 1.0);
        assert_eq!(p.va, (0, 0)); // the POC alone holds 83%
    }
}
