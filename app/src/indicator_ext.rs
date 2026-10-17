//! Worker-side build of the indicators added after the original set: ATR, Stochastic, ADX,
//! Ichimoku, OBV and anchored VWAP (the volume profile is drawn from the visible bars by the
//! chart, so it has no lines here).

use egui::Color32;
use tr_chart::gpu::LineKind;
use tr_core::Bar;
use tr_core::indicators_ext as ext;

use crate::command::IndicatorSpec;
use crate::theme::Palette;
use crate::worker::overlay;
use tr_chart::scene::Overlay;

/// The Ichimoku cloud: both leading spans, `kijun` bars longer than the bars.
pub struct Cloud {
    pub a: Vec<f64>,
    pub b: Vec<f64>,
}

pub struct Built {
    pub overlays: Vec<(String, Overlay)>,
    pub cloud: Option<Cloud>,
}

/// Lines for `spec`, or `None` for the original indicators (built in the worker).
pub fn build(spec: &IndicatorSpec, bars: &[Bar], palette: &Palette, color: Color32) -> Option<Built> {
    let lines = |overlays| Some(Built { overlays, cloud: None });
    match *spec {
        IndicatorSpec::Atr(n) => lines(vec![("ATR".into(), overlay(&ext::atr(bars, n), color, LineKind::Line))]),
        IndicatorSpec::Stoch(k, s, d) => {
            let (line, signal) = ext::stochastic(bars, k, s, d);
            lines(vec![
                ("%K".into(), overlay(&line, palette.cyan, LineKind::Line)),
                ("%D".into(), overlay(&signal, palette.amber, LineKind::Line)),
            ])
        }
        IndicatorSpec::Adx(n) => {
            let a = ext::adx(bars, n);
            lines(vec![
                ("ADX".into(), overlay(&a.adx, Color32::WHITE, LineKind::Line)),
                ("+DI".into(), overlay(&a.plus_di, palette.up, LineKind::Line)),
                ("-DI".into(), overlay(&a.minus_di, palette.down, LineKind::Line)),
            ])
        }
        IndicatorSpec::Ichimoku(t, k, s) => {
            let i = ext::ichimoku(bars, t, k, s);
            let len = bars.len();
            Some(Built {
                overlays: vec![
                    ("Tenkan".into(), overlay(&i.tenkan, palette.cyan, LineKind::Line)),
                    ("Kijun".into(), overlay(&i.kijun, palette.amber, LineKind::Line)),
                    ("Span A".into(), overlay(&i.senkou_a[..len], palette.up.gamma_multiply(0.7), LineKind::Line)),
                    ("Span B".into(), overlay(&i.senkou_b[..len], palette.down.gamma_multiply(0.7), LineKind::Line)),
                    ("Chikou".into(), overlay(&i.chikou, palette.series[4].gamma_multiply(0.8), LineKind::Line)),
                ],
                cloud: Some(Cloud { a: i.senkou_a, b: i.senkou_b }),
            })
        }
        IndicatorSpec::Obv => lines(vec![("OBV".into(), overlay(&ext::obv(bars), color, LineKind::Line))]),
        IndicatorSpec::AnchoredVwap(ts) => {
            let values = match ext::anchor_index(bars, ts) {
                Some(i) => ext::anchored_vwap(bars, i),
                None => vec![f64::NAN; bars.len()],
            };
            lines(vec![(spec.label(), overlay(&values, color, LineKind::Line))])
        }
        IndicatorSpec::VolumeProfile(_) => lines(Vec::new()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tr_core::Timeframe;

    fn palette() -> Palette {
        crate::themes::terminal_light().palette(crate::theme::DEFAULT_ACCENT)
    }

    #[test]
    fn builds_every_new_indicator_aligned_with_the_bars() {
        let bars = tr_core::sample::generate("X", Timeframe::D1, 120, 1_790_000_000);
        let p = palette();
        let names = |s: IndicatorSpec| -> Vec<String> {
            build(&s, &bars, &p, Color32::WHITE).expect("built here").overlays.into_iter().map(|o| o.0).collect()
        };
        assert_eq!(names(IndicatorSpec::Atr(14)), ["ATR"]);
        assert_eq!(names(IndicatorSpec::Stoch(14, 3, 3)), ["%K", "%D"]);
        assert_eq!(names(IndicatorSpec::Adx(14)), ["ADX", "+DI", "-DI"]);
        assert_eq!(names(IndicatorSpec::Ichimoku(9, 26, 52)).len(), 5);
        assert_eq!(names(IndicatorSpec::Obv), ["OBV"]);
        assert!(names(IndicatorSpec::VolumeProfile(48)).is_empty());
        let ich = build(&IndicatorSpec::Ichimoku(9, 26, 52), &bars, &p, Color32::WHITE).unwrap();
        assert_eq!(ich.cloud.expect("cloud").a.len(), 120 + 26, "the spans run kijun bars past the last bar");
        assert!(build(&IndicatorSpec::Sma(20), &bars, &p, Color32::WHITE).is_none());
    }

    #[test]
    fn anchored_vwap_starts_at_its_anchor() {
        let bars = tr_core::sample::generate("X", Timeframe::D1, 50, 1_790_000_000);
        let b = build(&IndicatorSpec::AnchoredVwap(bars[20].ts), &bars, &palette(), Color32::WHITE).unwrap();
        let data = &b.overlays[0].1.line.pyramid.data;
        assert!(data[19][1].is_nan() && data[20][1].is_finite());
        // An anchor past the data draws nothing rather than a wrong line.
        let none = build(&IndicatorSpec::AnchoredVwap(i64::MAX), &bars, &palette(), Color32::WHITE).unwrap();
        assert!(none.overlays[0].1.line.pyramid.data.iter().all(|p| p[1].is_nan()));
    }

    #[test]
    fn specs_survive_a_layout_round_trip() {
        let specs = vec![
            IndicatorSpec::Atr(10),
            IndicatorSpec::Stoch(5, 3, 3),
            IndicatorSpec::Adx(14),
            IndicatorSpec::Ichimoku(9, 26, 52),
            IndicatorSpec::Obv,
            IndicatorSpec::AnchoredVwap(1_790_000_000),
            IndicatorSpec::VolumeProfile(24),
        ];
        let json = serde_json::to_string(&specs).unwrap();
        assert_eq!(serde_json::from_str::<Vec<IndicatorSpec>>(&json).unwrap(), specs);
        assert!(specs.iter().filter(|s| s.is_pane()).count() == 4);
    }
}
