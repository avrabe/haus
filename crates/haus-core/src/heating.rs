//! Heating cost allocator usage and how it relates to temperature
//! (REQ-HEAT-001, DD-026): the numbers behind the Heizung page.
//!
//! An allocator counts units since the start of its billing period and
//! starts again from zero at the next one. A day's usage is the sum of the
//! rises of its readings; a drop is a new period, counted from zero.
//! Temperatures become daily means of the value standing at each full hour,
//! so a sensor that reports on change (Loxone) is weighted by time, not by
//! how often it changed.

use chrono::NaiveDate;
use std::collections::BTreeMap;

/// Units used per day. `day` maps a timestamp to its local date. A rise
/// counts on the day of the reading that shows it, so after a gap in the
/// readings the whole gap's usage lands on the day reception resumed.
pub fn daily_units(
    points: &[(i64, f64)],
    day: impl Fn(i64) -> NaiveDate,
) -> BTreeMap<NaiveDate, f64> {
    let mut out = BTreeMap::new();
    for w in points.windows(2) {
        let ((_, a), (t, b)) = (w[0], w[1]);
        let used = if b >= a { b - a } else { b };
        *out.entry(day(t)).or_insert(0.0) += used;
    }
    // A day with readings but no rise used nothing; say so explicitly.
    for (t, _) in points.iter().skip(1) {
        out.entry(day(*t)).or_insert(0.0);
    }
    out
}

/// Daily means of the value standing at each full hour in `[from, to)`. A
/// value stands for at most `hold` ms; an hour without a standing value is
/// left out, and a day needs at least 12 such hours to get a mean.
pub fn daily_means(
    points: &[(i64, f64)],
    from: i64,
    to: i64,
    hold: i64,
    day: impl Fn(i64) -> NaiveDate,
) -> BTreeMap<NaiveDate, f64> {
    const HOUR: i64 = 3_600_000;
    let mut sums: BTreeMap<NaiveDate, (f64, u32)> = BTreeMap::new();
    let mut i = 0;
    let mut last: Option<(i64, f64)> = None;
    let mut t = from.div_euclid(HOUR) * HOUR;
    if t < from {
        t += HOUR;
    }
    while t < to {
        while i < points.len() && points[i].0 <= t {
            last = Some(points[i]);
            i += 1;
        }
        if let Some((pt, v)) = last {
            if t - pt <= hold {
                let e = sums.entry(day(t)).or_insert((0.0, 0));
                e.0 += v;
                e.1 += 1;
            }
        }
        t += HOUR;
    }
    sums.into_iter()
        .filter(|(_, (_, n))| *n >= 12)
        .map(|(d, (s, n))| (d, s / n as f64))
        .collect()
}

/// A least-squares line through (x, y) pairs with Pearson's r.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub n: usize,
    /// Change in y per unit of x.
    pub slope: f64,
    pub intercept: f64,
    pub r: f64,
}

/// The fewest pairs a fit is shown for.
pub const MIN_DAYS: usize = 7;

/// The fit of `pairs`, or None with fewer than MIN_DAYS pairs or when either
/// side does not vary (r is undefined then).
pub fn fit(pairs: &[(f64, f64)]) -> Option<Fit> {
    let n = pairs.len();
    if n < MIN_DAYS {
        return None;
    }
    let nf = n as f64;
    let mx = pairs.iter().map(|p| p.0).sum::<f64>() / nf;
    let my = pairs.iter().map(|p| p.1).sum::<f64>() / nf;
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for (x, y) in pairs {
        sxx += (x - mx) * (x - mx);
        syy += (y - my) * (y - my);
        sxy += (x - mx) * (y - my);
    }
    if sxx <= f64::EPSILON || syy <= f64::EPSILON {
        return None;
    }
    let slope = sxy / sxx;
    Some(Fit {
        n,
        slope,
        intercept: my - slope * mx,
        r: sxy / (sxx * syy).sqrt(),
    })
}

/// The days both maps have, as (x, y) pairs.
pub fn pair(
    x: &BTreeMap<NaiveDate, f64>,
    y: &BTreeMap<NaiveDate, f64>,
) -> Vec<(NaiveDate, f64, f64)> {
    x.iter()
        .filter_map(|(d, a)| y.get(d).map(|b| (*d, *a, *b)))
        .collect()
}

/// Several daily maps summed per day (days missing in one count as absent
/// for that one, not as zero).
pub fn sum_days<'a>(
    maps: impl IntoIterator<Item = &'a BTreeMap<NaiveDate, f64>>,
) -> BTreeMap<NaiveDate, f64> {
    let mut out = BTreeMap::new();
    for m in maps {
        for (d, v) in m {
            *out.entry(*d).or_insert(0.0) += v;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    const D: i64 = 24 * H;

    fn utc_day(t: i64) -> NaiveDate {
        chrono::DateTime::from_timestamp_millis(t)
            .unwrap()
            .date_naive()
    }

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }

    // 2026-11-01T00:00:00Z
    const NOV1: i64 = 1_793_491_200_000;

    // rivet: verifies DD-026
    // rivet: verifies REQ-HEAT-001
    #[test]
    fn units_per_day_count_rises_and_a_new_period_from_zero() {
        assert_eq!(utc_day(NOV1), date(1));
        let p = [
            (NOV1 + H, 100.0),
            (NOV1 + 5 * H, 103.0),
            (NOV1 + 20 * H, 105.0),
            // Next day: no rise.
            (NOV1 + D + H, 105.0),
            // Day 3: a new period starts, the counter shows 2 since zero.
            (NOV1 + 2 * D + H, 107.0),
            (NOV1 + 2 * D + 2 * H, 2.0),
        ];
        let u = daily_units(&p, utc_day);
        assert_eq!(
            u.into_iter().collect::<Vec<_>>(),
            vec![(date(1), 5.0), (date(2), 0.0), (date(3), 4.0)]
        );
        assert!(daily_units(&p[..1], utc_day).is_empty());
    }

    // rivet: verifies DD-026
    // rivet: verifies REQ-HEAT-001
    #[test]
    fn daily_means_weigh_time_and_skip_thin_days() {
        // 18 °C from midnight, 22 °C from noon: 12 h each, mean 20 °C, not
        // the 21 °C a mean of the three points would give.
        let p = [
            (NOV1, 18.0),
            (NOV1 + 12 * H, 22.0),
            (NOV1 + 12 * H + 1, 22.0),
        ];
        let m = daily_means(&p, NOV1, NOV1 + 2 * D, 6 * H, utc_day);
        // Day 2: the value goes stale after 6 h, so 7 hours: too thin.
        assert_eq!(m.into_iter().collect::<Vec<_>>(), vec![(date(1), 20.0)]);
    }

    // rivet: verifies REQ-HEAT-001
    #[test]
    fn the_fit_needs_a_week_and_finds_the_line() {
        // y = 30 - 2x exactly: r = -1.
        let p: Vec<(f64, f64)> = (0..7).map(|x| (x as f64, 30.0 - 2.0 * x as f64)).collect();
        let f = fit(&p).unwrap();
        assert_eq!(f.n, 7);
        assert!((f.slope + 2.0).abs() < 1e-9 && (f.intercept - 30.0).abs() < 1e-9);
        assert!((f.r + 1.0).abs() < 1e-9);
        assert!(fit(&p[..6]).is_none());
        // No variation: no fit rather than NaN.
        assert!(fit(&[(1.0, 5.0); 8]).is_none());
        let a: BTreeMap<_, _> = [(date(1), 1.0), (date(2), 2.0)].into();
        let b: BTreeMap<_, _> = [(date(2), 5.0), (date(3), 6.0)].into();
        assert_eq!(pair(&a, &b), vec![(date(2), 2.0, 5.0)]);
        assert_eq!(
            sum_days([&a, &b]).into_iter().collect::<Vec<_>>(),
            vec![(date(1), 1.0), (date(2), 7.0), (date(3), 6.0)]
        );
    }
}
