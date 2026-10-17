//! User drawings. Anchors are stored as (timestamp, price) so they survive pan, zoom and
//! timeframe changes; they are mapped to bar indices only when drawn.

use serde::{Deserialize, Serialize};

pub const FIB_LEVELS: [f64; 7] = [0.0, 0.236, 0.382, 0.5, 0.618, 0.786, 1.0];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub ts: i64,
    pub price: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tool {
    Trendline,
    Horizontal,
    Vertical,
    Fib,
    Rectangle,
    /// A line from `a` through `b`, running on to the right edge.
    Ray,
    /// Two parallel lines: `a`-`b` and the one through `c`.
    Channel,
    /// A text label at `a`.
    Text,
    /// Price, percent, bar and time distance between `a` and `b`.
    Measure,
    /// Entry `a`, stop `b`, target `c` (default 2R) of a long trade.
    Long,
    /// The same for a short trade.
    Short,
}

impl Tool {
    pub const ALL: [Tool; 11] = [
        Tool::Trendline,
        Tool::Horizontal,
        Tool::Vertical,
        Tool::Fib,
        Tool::Rectangle,
        Tool::Ray,
        Tool::Channel,
        Tool::Text,
        Tool::Measure,
        Tool::Long,
        Tool::Short,
    ];

    /// Clicks needed to place the drawing.
    pub fn points(self) -> usize {
        match self {
            Tool::Horizontal | Tool::Vertical | Tool::Text => 1,
            Tool::Trendline | Tool::Fib | Tool::Rectangle | Tool::Ray | Tool::Measure => 2,
            Tool::Channel | Tool::Long | Tool::Short => 3,
        }
    }

    /// Whether the third point is optional (the position tools default their target).
    pub fn is_position(self) -> bool {
        matches!(self, Tool::Long | Tool::Short)
    }

    /// Tools added after the original four; the chart paints these itself.
    pub fn is_extra(self) -> bool {
        !matches!(self, Tool::Trendline | Tool::Horizontal | Tool::Vertical | Tool::Fib)
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::Trendline => "Trendline",
            Tool::Horizontal => "Horizontal line",
            Tool::Vertical => "Vertical line",
            Tool::Fib => "Fib retracement",
            Tool::Rectangle => "Rectangle",
            Tool::Ray => "Ray",
            Tool::Channel => "Parallel channel",
            Tool::Text => "Text note",
            Tool::Measure => "Measure",
            Tool::Long => "Long position",
            Tool::Short => "Short position",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drawing {
    pub tool: Tool,
    pub a: Anchor,
    pub b: Anchor,
    /// Third point (channel offset, position target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c: Option<Anchor>,
    /// Text of a note.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Custom colour (RGB); `None` = the theme's drawing colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
}

impl Drawing {
    pub fn new(tool: Tool, a: Anchor, b: Anchor) -> Self {
        Self { tool, a, b, c: None, text: String::new(), color: None }
    }

    /// A drawing from the anchors placed so far; missing points repeat the last one.
    pub fn from_points(tool: Tool, pts: &[Anchor]) -> Option<Self> {
        let a = *pts.first()?;
        let b = pts.get(1).copied().unwrap_or(a);
        let mut d = Self::new(tool, a, b);
        d.c = pts.get(2).copied();
        if tool == Tool::Text {
            d.text = "Note".into();
        }
        Some(d)
    }

    /// Every anchor of the drawing, mutably (for moving it).
    pub fn anchors_mut(&mut self) -> Vec<&mut Anchor> {
        let mut v = vec![&mut self.a, &mut self.b];
        v.extend(self.c.as_mut());
        v
    }

    /// Target of a position drawing: the placed one, or 2R from the entry.
    pub fn target_price(&self) -> f64 {
        self.c.map_or_else(|| default_target(self.tool == Tool::Long, self.a.price, self.b.price), |c| c.price)
    }

    /// Fib levels as `(ratio, price)`, measured from `b` (0) back to `a` (1), the
    /// usual retracement convention for a swing drawn from `a` to `b`.
    pub fn fib_levels(&self) -> Vec<(f64, f64)> {
        FIB_LEVELS.iter().map(|&r| (r, self.b.price + (self.a.price - self.b.price) * r)).collect()
    }
}

/// Colours offered for drawings.
pub const PALETTE: [(&str, [u8; 3]); 7] = [
    ("Cyan", [0, 188, 212]),
    ("Amber", [255, 176, 0]),
    ("Green", [38, 166, 91]),
    ("Red", [229, 57, 53]),
    ("Blue", [66, 133, 244]),
    ("Purple", [171, 71, 188]),
    ("White", [235, 235, 235]),
];

/// Default risk:reward of a position tool whose target was not placed.
pub const DEFAULT_RR: f64 = 2.0;

pub fn default_target(long: bool, entry: f64, stop: f64) -> f64 {
    let risk = (entry - stop).abs();
    if long { entry + DEFAULT_RR * risk } else { entry - DEFAULT_RR * risk }
}

/// Distance between two anchors for the measure tool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    pub delta: f64,
    pub pct: f64,
    pub bars: i64,
    pub secs: i64,
}

pub fn measure(a: Anchor, b: Anchor, bars: i64) -> Measurement {
    let delta = b.price - a.price;
    let pct = if a.price != 0.0 { delta / a.price * 100.0 } else { 0.0 };
    Measurement { delta, pct, bars, secs: b.ts - a.ts }
}

/// `3d 4h`, `2h 5m`, `45s`: the two most significant units of a (non-negative) span.
pub fn format_span(secs: i64) -> String {
    let s = secs.unsigned_abs();
    let (d, h, m) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60);
    let sign = if secs < 0 { "-" } else { "" };
    if d > 0 {
        format!("{sign}{d}d {h}h")
    } else if h > 0 {
        format!("{sign}{h}h {m}m")
    } else if m > 0 {
        format!("{sign}{m}m")
    } else {
        format!("{sign}{s}s")
    }
}

/// Risk / reward numbers of a long or short position drawing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub long: bool,
    pub entry: f64,
    pub stop: f64,
    pub target: f64,
    /// Price distance risked (always positive).
    pub risk: f64,
    /// Price distance to the target in the trade's direction (negative if on the wrong side).
    pub reward: f64,
    /// Reward over risk; `None` when there is no risk.
    pub rr: Option<f64>,
    /// Stop and target as signed percent moves from the entry, in the trade's favour.
    pub stop_pct: f64,
    pub target_pct: f64,
}

pub fn position(long: bool, entry: f64, stop: f64, target: f64) -> Position {
    let dir = if long { 1.0 } else { -1.0 };
    let risk = (entry - stop).abs();
    let reward = (target - entry) * dir;
    let pct = |p: f64| if entry != 0.0 { (p - entry) / entry * 100.0 * dir } else { 0.0 };
    Position {
        long,
        entry,
        stop,
        target,
        risk,
        reward,
        rr: (risk > 0.0).then(|| reward / risk),
        stop_pct: pct(stop),
        target_pct: pct(target),
    }
}

impl Drawing {
    /// Numbers of a position drawing (`None` for other tools).
    pub fn position(&self) -> Option<Position> {
        self.tool
            .is_position()
            .then(|| position(self.tool == Tool::Long, self.a.price, self.b.price, self.target_price()))
    }
}

/// Screen-space geometry of a drawing, produced by the chart for hit testing.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Segment([f64; 2], [f64; 2]),
    HLine(f64),
    VLine(f64),
}

/// Distance in pixels from point `p` to a shape.
pub fn distance(shape: &Shape, p: [f64; 2]) -> f64 {
    match shape {
        Shape::HLine(y) => (p[1] - y).abs(),
        Shape::VLine(x) => (p[0] - x).abs(),
        Shape::Segment(a, b) => {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let len2 = dx * dx + dy * dy;
            let t = if len2 == 0.0 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) };
            let (cx, cy) = (a[0] + t * dx, a[1] + t * dy);
            ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt()
        }
    }
}

/// Snap `price` to the nearest of a bar's O/H/L/C (magnet mode).
pub fn magnet(price: f64, ohlc: [f64; 4]) -> f64 {
    ohlc.into_iter().min_by(|a, b| (a - price).abs().total_cmp(&(b - price).abs())).unwrap_or(price)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fib_levels_retrace_from_b() {
        let d = Drawing::new(Tool::Fib, Anchor { ts: 0, price: 100.0 }, Anchor { ts: 1, price: 200.0 });
        let lv = d.fib_levels();
        assert_eq!(lv[0], (0.0, 200.0));
        assert_eq!(lv[3], (0.5, 150.0));
        assert!((lv[4].1 - 138.2).abs() < 1e-9);
        assert_eq!(lv[6], (1.0, 100.0));
    }

    #[test]
    fn measure_numbers() {
        let m = measure(Anchor { ts: 0, price: 100.0 }, Anchor { ts: 90_000, price: 110.0 }, 5);
        assert_eq!((m.delta, m.pct, m.bars, m.secs), (10.0, 10.0, 5, 90_000));
        assert_eq!(format_span(90_000), "1d 1h");
        assert_eq!(format_span(7_500), "2h 5m");
        assert_eq!(format_span(-45), "-45s");
        assert_eq!(measure(Anchor { ts: 0, price: 0.0 }, Anchor { ts: 1, price: 1.0 }, 1).pct, 0.0);
    }

    #[test]
    fn position_risk_reward() {
        // Long from 100, stop 95, target 110: risk 5, reward 10, 2R.
        let p = position(true, 100.0, 95.0, 110.0);
        assert_eq!((p.risk, p.reward, p.rr), (5.0, 10.0, Some(2.0)));
        assert_eq!((p.stop_pct, p.target_pct), (-5.0, 10.0));
        // Short from 100, stop 104, target 92: risk 4, reward 8, both percents in its favour.
        let s = position(false, 100.0, 104.0, 92.0);
        assert_eq!((s.risk, s.reward, s.rr), (4.0, 8.0, Some(2.0)));
        assert_eq!((s.stop_pct, s.target_pct), (-4.0, 8.0));
        // A target on the wrong side is a negative reward, and no stop distance has no ratio.
        assert_eq!(position(true, 100.0, 95.0, 98.0).reward, -2.0);
        assert_eq!(position(true, 100.0, 100.0, 110.0).rr, None);
    }

    #[test]
    fn position_default_target_is_two_r() {
        let mut d =
            Drawing::from_points(Tool::Long, &[Anchor { ts: 0, price: 50.0 }, Anchor { ts: 5, price: 48.0 }]).unwrap();
        assert_eq!(d.target_price(), 54.0);
        d.c = Some(Anchor { ts: 5, price: 60.0 });
        assert_eq!(d.position().unwrap().rr, Some(5.0));
        let short = Drawing::new(Tool::Short, Anchor { ts: 0, price: 50.0 }, Anchor { ts: 5, price: 52.0 });
        assert_eq!(short.target_price(), 46.0);
        assert!(Drawing::new(Tool::Fib, short.a, short.b).position().is_none());
    }

    #[test]
    fn old_drawings_still_load_and_new_fields_round_trip() {
        let old = r#"{"tool":"Trendline","a":{"ts":1,"price":2.0},"b":{"ts":3,"price":4.0}}"#;
        let d: Drawing = serde_json::from_str(old).unwrap();
        assert_eq!((d.c, d.text.as_str(), d.color), (None, "", None));
        assert_eq!(serde_json::to_string(&d).unwrap(), old);
        let mut n = Drawing::from_points(Tool::Text, &[d.a]).unwrap();
        n.color = Some([1, 2, 3]);
        let back: Drawing = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
        assert_eq!(back, n);
        assert_eq!(back.text, "Note");
    }

    #[test]
    fn hit_distances() {
        assert_eq!(distance(&Shape::Segment([0.0, 0.0], [10.0, 0.0]), [5.0, 3.0]), 3.0);
        assert_eq!(distance(&Shape::Segment([0.0, 0.0], [10.0, 0.0]), [13.0, 4.0]), 5.0);
        assert_eq!(distance(&Shape::HLine(50.0), [999.0, 47.0]), 3.0);
        assert_eq!(distance(&Shape::VLine(10.0), [12.0, 0.0]), 2.0);
    }

    #[test]
    fn magnet_snaps() {
        assert_eq!(magnet(10.4, [10.0, 12.0, 9.0, 11.0]), 10.0);
        assert_eq!(magnet(11.7, [10.0, 12.0, 9.0, 11.0]), 12.0);
    }
}
