//! When a motion sensor sees movement, by weekday and hour (REQ-ANA-003,
//! DD-027).
//!
//! A PIR sensor reports a running movement counter. The movements between
//! two readings belong to the hour of the later one, as long as the
//! readings are at most an hour apart; a longer gap cannot be placed in an
//! hour and is left out. A counter that drops restarted (battery change,
//! 15-bit wrap) and counts from zero.

/// The longest gap whose movements are still placed in an hour.
pub const MAX_GAP_MS: i64 = 3_600_000;

/// Mean movements per weekday (0 = Monday) and hour, over the days that
/// had readings in that slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    pub mean: [[f64; 24]; 7],
    /// Days with at least one placed reading.
    pub days: usize,
}

/// The pattern of `points` (sorted by time). `slot` maps a timestamp to its
/// local (date as days since the epoch, weekday 0..7, hour 0..24).
pub fn weekly(points: &[(i64, f64)], slot: impl Fn(i64) -> (i64, usize, usize)) -> Pattern {
    let mut sum = [[0.0; 24]; 7];
    // Which dates contributed to each weekday: a weekday's mean divides by
    // its own number of observed days.
    let mut dates: [std::collections::BTreeSet<i64>; 7] = Default::default();
    for w in points.windows(2) {
        let ((t0, a), (t1, b)) = (w[0], w[1]);
        if t1 - t0 > MAX_GAP_MS || !(a.is_finite() && b.is_finite()) {
            continue;
        }
        let moved = if b >= a { b - a } else { b };
        let (date, wd, h) = slot(t1);
        sum[wd][h] += moved;
        dates[wd].insert(date);
    }
    let mut mean = [[0.0; 24]; 7];
    for wd in 0..7 {
        let n = dates[wd].len();
        if n > 0 {
            for h in 0..24 {
                mean[wd][h] = sum[wd][h] / n as f64;
            }
        }
    }
    Pattern {
        mean,
        days: dates.iter().map(|d| d.len()).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    const D: i64 = 24 * H;

    /// UTC slots; day 0 (the epoch) was a Thursday (weekday 3).
    fn utc(t: i64) -> (i64, usize, usize) {
        let day = t.div_euclid(D);
        (
            day,
            ((day + 3) % 7) as usize,
            (t.rem_euclid(D) / H) as usize,
        )
    }

    // rivet: verifies DD-027
    // rivet: verifies REQ-ANA-003
    #[test]
    fn movements_land_in_their_weekday_and_hour() {
        // Two Thursdays (day 0 and day 7): 10 movements at 08:xx on both,
        // 4 at 20:xx on the first only. Readings every 30 min.
        let mut p = vec![];
        for (day, base) in [(0, 100.0), (7, 500.0)] {
            let t = day * D;
            p.push((t + 7 * H + 50 * 60_000, base));
            p.push((t + 8 * H + 20 * 60_000, base + 10.0));
            p.push((t + 20 * H + 10 * 60_000, base + 10.0)); // gap > 1 h: skipped
            p.push((
                t + 20 * H + 40 * 60_000,
                base + if day == 0 { 14.0 } else { 10.0 },
            ));
        }
        let pat = weekly(&p, utc);
        assert_eq!(pat.mean[3][8], 10.0);
        assert_eq!(pat.mean[3][20], 2.0);
        assert_eq!(pat.days, 2);
        // The jump from day 0's last reading to day 7's first is a gap.
        assert_eq!(pat.mean[3].iter().sum::<f64>(), 12.0);
        assert!(pat.mean[0].iter().all(|v| *v == 0.0));
    }

    // rivet: verifies REQ-ANA-003
    #[test]
    fn a_restarted_counter_counts_from_zero() {
        let p = [(10 * H, 32_760.0), (10 * H + 20 * 60_000, 3.0)];
        let pat = weekly(&p, utc);
        assert_eq!(pat.mean[3][10], 3.0);
        assert_eq!(weekly(&p[..1], utc).days, 0);
    }
}
