//! Plug-in chart studies.
//!
//! A [`Study`] turns a chart's bars into things to draw: lines and fills on the price pane,
//! markers, segments, sub-panes and a summary table. The app computes studies on its worker
//! thread and draws the output generically, so a study needs no UI code of its own.
//!
//! Studies are registered when the app starts (`terminal_rs::run(studies)`), which lets a
//! separate (even private) crate build the terminal with its own studies without this
//! repository depending on it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tr_core::{Bar, Timeframe};

/// Straight (non-premultiplied) RGBA.
pub type Rgba = [u8; 4];

pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    [r, g, b, 255]
}

/// `color` with opacity `alpha` (0–255).
pub const fn with_alpha(color: Rgba, alpha: u8) -> Rgba {
    [color[0], color[1], color[2], alpha]
}

// ---- settings --------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamKind {
    Bool,
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64, step: f64 },
    Choice(&'static [&'static str]),
}

/// One user setting of a study.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    /// Stable key (persisted in layouts).
    pub key: &'static str,
    pub label: &'static str,
    /// Settings are grouped under this heading in the menu.
    pub group: &'static str,
    pub kind: ParamKind,
    pub default: ParamValue,
    pub help: &'static str,
}

impl Param {
    pub const fn bool(key: &'static str, label: &'static str, group: &'static str, default: bool) -> Self {
        Param { key, label, group, kind: ParamKind::Bool, default: ParamValue::Bool(default), help: "" }
    }

    pub const fn int(
        key: &'static str,
        label: &'static str,
        group: &'static str,
        default: i64,
        min: i64,
        max: i64,
    ) -> Self {
        Param { key, label, group, kind: ParamKind::Int { min, max }, default: ParamValue::Int(default), help: "" }
    }

    pub const fn float(
        key: &'static str,
        label: &'static str,
        group: &'static str,
        default: f64,
        min: f64,
        max: f64,
        step: f64,
    ) -> Self {
        Param {
            key,
            label,
            group,
            kind: ParamKind::Float { min, max, step },
            default: ParamValue::Float(default),
            help: "",
        }
    }

    /// `options[0]` is not implied; `default` must be one of `options`.
    pub fn choice(
        key: &'static str,
        label: &'static str,
        group: &'static str,
        options: &'static [&'static str],
        default: &str,
    ) -> Self {
        Param {
            key,
            label,
            group,
            kind: ParamKind::Choice(options),
            default: ParamValue::Text(default.into()),
            help: "",
        }
    }

    pub const fn help(mut self, help: &'static str) -> Self {
        self.help = help;
        self
    }

    /// `v` if it fits this parameter (type and range), otherwise the default.
    pub fn sanitize(&self, v: Option<&ParamValue>) -> ParamValue {
        match (&self.kind, v) {
            (ParamKind::Bool, Some(ParamValue::Bool(b))) => ParamValue::Bool(*b),
            (ParamKind::Int { min, max }, Some(ParamValue::Int(i))) => ParamValue::Int((*i).clamp(*min, *max)),
            (ParamKind::Float { min, max, .. }, Some(ParamValue::Float(f))) if f.is_finite() => {
                ParamValue::Float(f.clamp(*min, *max))
            }
            (ParamKind::Float { min, max, .. }, Some(ParamValue::Int(i))) => {
                ParamValue::Float((*i as f64).clamp(*min, *max))
            }
            (ParamKind::Choice(opts), Some(ParamValue::Text(s))) if opts.contains(&s.as_str()) => {
                ParamValue::Text(s.clone())
            }
            _ => self.default.clone(),
        }
    }
}

/// Stored values by [`Param::key`]; missing keys take the default.
pub type Params = BTreeMap<String, ParamValue>;

/// Typed, defaulted access to a study's settings.
#[derive(Clone, Copy)]
pub struct ParamsView<'a> {
    defs: &'a [Param],
    values: &'a Params,
}

impl<'a> ParamsView<'a> {
    pub fn new(defs: &'a [Param], values: &'a Params) -> Self {
        Self { defs, values }
    }

    fn get(&self, key: &str) -> ParamValue {
        match self.defs.iter().find(|p| p.key == key) {
            Some(p) => p.sanitize(self.values.get(key)),
            None => panic!("study parameter `{key}` is not declared"),
        }
    }

    pub fn bool(&self, key: &str) -> bool {
        matches!(self.get(key), ParamValue::Bool(true))
    }

    pub fn int(&self, key: &str) -> i64 {
        match self.get(key) {
            ParamValue::Int(i) => i,
            _ => 0,
        }
    }

    pub fn float(&self, key: &str) -> f64 {
        match self.get(key) {
            ParamValue::Float(f) => f,
            ParamValue::Int(i) => i as f64,
            _ => 0.0,
        }
    }

    pub fn text(&self, key: &str) -> String {
        match self.get(key) {
            ParamValue::Text(s) => s,
            _ => String::new(),
        }
    }
}

// ---- output ----------------------------------------------------------------------------

/// A per-bar series. NaN breaks the line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub name: String,
    pub values: Vec<f64>,
    pub color: Rgba,
    /// Columns from zero instead of a line (sub-panes).
    pub histogram: bool,
    /// Show the value in the legend.
    pub legend: bool,
}

impl Line {
    pub fn new(name: impl Into<String>, values: Vec<f64>, color: Rgba) -> Self {
        Line { name: name.into(), values, color, histogram: false, legend: true }
    }

    pub fn histogram(mut self) -> Self {
        self.histogram = true;
        self
    }

    pub fn quiet(mut self) -> Self {
        self.legend = false;
        self
    }
}

/// Shaded area between two per-bar series; bars where either is NaN are left open.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub a: Vec<f64>,
    pub b: Vec<f64>,
    pub color: Rgba,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerShape {
    /// Label box under the bar's low, pointing up.
    LabelUp,
    /// Label box above the bar's high, pointing down.
    LabelDown,
    /// Small triangle under the low.
    TriangleUp,
    /// Small triangle above the high.
    TriangleDown,
    /// Diamond under the low (`below`) or above the high.
    Diamond { below: bool },
    /// Dot at `price`.
    Dot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub bar: usize,
    /// Used by [`MarkerShape::Dot`]; the other shapes anchor to the bar's high/low.
    pub price: f64,
    pub shape: MarkerShape,
    pub text: String,
    pub color: Rgba,
    pub tooltip: String,
}

/// A straight line between two (bar, price) points.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub from: (usize, f64),
    pub to: (usize, f64),
    pub color: Rgba,
    pub dashed: bool,
    pub width: f32,
}

/// An oscillator sub-pane under the price pane.
#[derive(Debug, Clone, PartialEq)]
pub struct Pane {
    pub title: String,
    pub lines: Vec<Line>,
    /// Horizontal reference levels.
    pub levels: Vec<(f64, Rgba)>,
    /// Fixed value range; `None` autoscales to the visible data.
    pub range: Option<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cell {
    pub text: String,
    /// `None` = the theme's text colour.
    pub color: Option<Rgba>,
    pub background: Option<Rgba>,
    pub tooltip: String,
}

impl Cell {
    pub fn new(text: impl Into<String>) -> Self {
        Cell { text: text.into(), ..Default::default() }
    }

    pub fn color(mut self, c: Rgba) -> Self {
        self.color = Some(c);
        self
    }

    pub fn background(mut self, c: Rgba) -> Self {
        self.background = Some(c);
        self
    }

    pub fn tooltip(mut self, t: impl Into<String>) -> Self {
        self.tooltip = t.into();
        self
    }
}

/// A small table drawn in the price pane's corner (state of the last bar).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Table {
    pub rows: Vec<Vec<Cell>>,
}

/// Everything a study draws.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Output {
    pub lines: Vec<Line>,
    pub fills: Vec<Fill>,
    pub markers: Vec<Marker>,
    pub segments: Vec<Segment>,
    pub panes: Vec<Pane>,
    pub table: Option<Table>,
    /// Short state shown in the legend next to the study name (e.g. the current decision).
    pub summary: Option<Cell>,
}

// ---- the trait -------------------------------------------------------------------------

/// Input for one computation.
pub struct Input<'a> {
    /// Market key as shown by the terminal (`AAPL US Equity`).
    pub symbol: &'a str,
    pub tf: Timeframe,
    /// Oldest first; the last bar may still be forming.
    pub bars: &'a [Bar],
    pub params: ParamsView<'a>,
}

pub trait Study: Send + Sync + 'static {
    /// Stable identifier, persisted in layouts.
    fn id(&self) -> &str;
    /// Display name.
    fn name(&self) -> &str;
    /// Command-line function that toggles the study on the focused chart (e.g. `C11`).
    fn command(&self) -> Option<&str> {
        None
    }
    fn description(&self) -> &str {
        ""
    }
    fn params(&self) -> &[Param] {
        &[]
    }
    fn compute(&self, input: &Input<'_>) -> Output;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_fall_back_to_defaults_and_clamp() {
        let defs = [
            Param::int("n", "N", "", 7, 1, 11),
            Param::bool("on", "On", "", true),
            Param::choice("m", "Mode", "", &["a", "b"], "a"),
        ];
        let mut values = Params::new();
        values.insert("n".into(), ParamValue::Int(40));
        values.insert("m".into(), ParamValue::Text("zzz".into()));
        let v = ParamsView::new(&defs, &values);
        assert_eq!(v.int("n"), 11);
        assert!(v.bool("on"));
        assert_eq!(v.text("m"), "a");
    }

    #[test]
    fn param_values_round_trip_untagged() {
        let mut p = Params::new();
        p.insert("a".into(), ParamValue::Bool(true));
        p.insert("b".into(), ParamValue::Int(3));
        p.insert("c".into(), ParamValue::Float(0.5));
        p.insert("d".into(), ParamValue::Text("x".into()));
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(s, r#"{"a":true,"b":3,"c":0.5,"d":"x"}"#);
        assert_eq!(serde_json::from_str::<Params>(&s).unwrap(), p);
    }
}
