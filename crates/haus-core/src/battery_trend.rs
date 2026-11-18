//! When a sensor's battery will need replacing, from its trend
//! (REQ-ANA-002, DD-027).
//!
//! The voltage swings with temperature from hour to hour, so the trend is
//! a straight line through the daily medians. Lithium cells hold their
//! voltage almost flat and then fall within weeks, so a flat line means
//! "no decline yet", never a promise; the "battery low" check stays the
//! alarm.

use crate::heating::fit;
use std::collections::BTreeMap;

const DAY: i64 = 86_400_000;
/// History a trend needs: two weeks between the first and the last day.
pub const MIN_SPAN_DAYS: i64 = 14;
/// A decline slower than this (per 30 days) counts as none: below the
/// sensors' 10 mV resolution over a month.
pub const FLAT_PER_MONTH: f64 = 0.005;
/// A forecast further out than this is shown as "more than five years".
pub const HORIZON_DAYS: f64 = 5.0 * 365.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Forecast {
    /// Days with readings the trend uses.
    pub days: usize,
    /// Change per 30 days, in the series' unit (negative: declining).
    pub per_month: f64,
    /// When the trend reaches the low threshold: None while there is no
    /// decline or it is further out than HORIZON_DAYS.
    pub low_at_ms: Option<i64>,
}

/// The trend of a battery series against its `low` threshold, or None with
/// less than MIN_SPAN_DAYS of history.
pub fn forecast(points: &[(i64, f64)], low: f64, now_ms: i64) -> Option<Forecast> {
    let mut by_day: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for (t, v) in points.iter().filter(|(_, v)| v.is_finite()) {
        by_day.entry(t.div_euclid(DAY)).or_default().push(*v);
    }
    let (first, last) = (*by_day.keys().next()?, *by_day.keys().next_back()?);
    if last - first < MIN_SPAN_DAYS {
        return None;
    }
    let pairs: Vec<(f64, f64)> = by_day
        .into_iter()
        .map(|(d, mut v)| {
            v.sort_by(|a, b| a.total_cmp(b));
            ((d - first) as f64, v[v.len() / 2])
        })
        .collect();
    let days = pairs.len();
    // A perfectly flat history has no fit (r undefined): no decline.
    let (slope, intercept) = match fit(&pairs) {
        Some(f) => (f.slope, f.intercept),
        None => (0.0, pairs[0].1),
    };
    let per_month = slope * 30.0;
    let low_at_ms = if per_month > -FLAT_PER_MONTH {
        None
    } else {
        let today = (now_ms.div_euclid(DAY) - first) as f64;
        let left = ((low - intercept) / slope - today).max(0.0);
        (left <= HORIZON_DAYS).then(|| now_ms + (left * DAY as f64) as i64)
    };
    Some(Forecast {
        days,
        per_month,
        low_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Midnight UTC, so a day's six readings fall on one day.
    const T0: i64 = 1_789_862_400_000;

    /// Six readings a day for `n` days, starting at `v0` and changing by
    /// `per_day`, with a ±0.03 V daily swing the medians must ignore.
    fn series(n: i64, v0: f64, per_day: f64) -> Vec<(i64, f64)> {
        (0..n)
            .flat_map(|d| {
                (0..6).map(move |k| {
                    let swing = [0.0, 0.03, -0.03, 0.01, -0.01, 0.0][k as usize];
                    (
                        T0 + d * DAY + k * 4 * 3_600_000,
                        v0 + per_day * d as f64 + swing,
                    )
                })
            })
            .collect()
    }

    // rivet: verifies REQ-ANA-002
    #[test]
    fn a_steady_decline_gives_the_day_it_reaches_the_threshold() {
        // 3.0 V falling 0.01 V/day for 30 days: 2.71 V on day 29, so 2.6 V
        // is 11 days after the last day.
        let p = series(30, 3.0, -0.01);
        let now = T0 + 29 * DAY;
        let f = forecast(&p, 2.6, now).unwrap();
        assert_eq!(f.days, 30);
        assert!((f.per_month + 0.3).abs() < 1e-6, "{}", f.per_month);
        let left = (f.low_at_ms.unwrap() - now) as f64 / DAY as f64;
        assert!((left - 11.0).abs() < 0.5, "{left}");
        // Already below: now.
        assert_eq!(forecast(&p, 2.9, now).unwrap().low_at_ms, Some(now));
    }

    // rivet: verifies REQ-ANA-002
    #[test]
    fn flat_far_or_short_histories_give_no_date() {
        let now = T0 + 60 * DAY;
        // Flat: no decline, no date.
        let f = forecast(&series(60, 3.0, 0.0), 2.6, now).unwrap();
        assert_eq!(f.low_at_ms, None);
        assert!(f.per_month.abs() < 1e-9);
        // 0.0001 V/day = 0.003 V/month: below the flat limit.
        assert_eq!(
            forecast(&series(60, 3.0, -0.0001), 2.6, now)
                .unwrap()
                .low_at_ms,
            None
        );
        // 0.0002 V/day from 3.0 V: about 2000 days, past five years.
        assert_eq!(
            forecast(&series(60, 3.0, -0.0002), 2.6, now)
                .unwrap()
                .low_at_ms,
            None
        );
        // 13 days of span: too short.
        assert!(forecast(&series(14, 3.0, -0.01), 2.6, now).is_none());
        assert!(forecast(&series(15, 3.0, -0.01), 2.6, now).is_some());
        assert!(forecast(&[], 2.6, now).is_none());
    }
}
