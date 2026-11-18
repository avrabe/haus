//! The Heizung page (REQ-HEAT-001, DD-026): what the heating cost
//! allocators counted, per room and day, and how that relates to the
//! outside temperature, the room temperature and haus's own sensors.

use crate::dashboard::{esc, now_ms, page};
use crate::state::{run_blocking, AppState};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chrono::NaiveDate;
use haus_core::heating::{daily_means, daily_units, fit, pair, sum_days, Fit, MIN_DAYS};
use haus_core::store::{Quantity, Source};
use haus_core::warn;
use std::collections::BTreeMap;
use std::fmt::Write as _;

type Days = BTreeMap<NaiveDate, f64>;

const DAY: i64 = 86_400_000;
/// History the fits use.
const FIT_DAYS: i64 = 90;
/// Days the bar chart shows.
const CHART_DAYS: i64 = 30;
/// How long a temperature stands without a new point for the daily means.
const HOLD: i64 = 6 * 3_600_000;

/// One allocator with what it counted.
#[derive(Clone, Debug, Default)]
pub struct Meter {
    pub name: String,
    pub room: String,
    pub units: BTreeMap<NaiveDate, f64>,
    /// The counter now: the units of the current billing period.
    pub period: Option<f64>,
    pub est_c: Option<f64>,
    pub radiator_c: Option<f64>,
}

/// Everything the page shows, prepared from the store.
#[derive(Clone, Debug, Default)]
pub struct Input {
    pub today: NaiveDate,
    pub meters: Vec<Meter>,
    /// Daily mean outside temperature; empty when no room is marked outdoor.
    pub outside: BTreeMap<NaiveDate, f64>,
    pub has_outside: bool,
    /// Daily mean room temperature by haus's own air sensors, per room.
    pub room_c: BTreeMap<String, BTreeMap<NaiveDate, f64>>,
    /// The room temperature by haus's own air sensors now.
    pub room_now: BTreeMap<String, f64>,
}

fn local_day(t: i64) -> NaiveDate {
    chrono::DateTime::from_timestamp_millis(t)
        .map(|d| d.with_timezone(&chrono::Local).date_naive())
        .unwrap_or_default()
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// Read the allocators, the outdoor and the room temperatures; blocking.
fn load(s: &AppState, now: i64) -> Result<Input, String> {
    let store = s.ingest.store();
    let from = now - FIT_DAYS * DAY;
    let all = store.series().map_err(|e| e.to_string())?;
    let points = |id| -> Result<Vec<(i64, f64)>, String> {
        let mut p: Vec<(i64, f64)> = store
            .before(id, from)
            .map_err(|e| e.to_string())?
            .into_iter()
            .collect();
        p.extend(store.range(id, from, now).map_err(|e| e.to_string())?);
        Ok(p)
    };
    let mut meters: BTreeMap<String, Meter> = BTreeMap::new();
    let latest = |id| store.latest(id).ok().flatten().map(|(_, v)| v);
    for (id, m) in all.iter().filter(|(_, m)| m.key.source == Source::Wmbus) {
        let e = meters.entry(m.key.source_id.clone()).or_default();
        e.room = m.room.clone().unwrap_or_default();
        match m.key.field.as_str() {
            "current_hca" => {
                e.name = m
                    .name
                    .trim_start_matches("Heizkostenverteiler ")
                    .to_string();
                e.units = daily_units(&points(*id)?, local_day);
                e.period = latest(*id);
            }
            "temp_room_c" => e.est_c = latest(*id),
            "temp_radiator_c" => e.radiator_c = latest(*id),
            _ => {}
        }
    }
    let meters: Vec<Meter> = meters
        .into_values()
        .filter(|m| !m.name.is_empty())
        .collect();
    let outdoor = |room: &str| s.warn.rooms.get(room).is_some_and(|r| r.outdoor);
    let air = |m: &haus_core::store::SeriesMeta| {
        m.key.source != Source::Wmbus && warn::is_air(&s.warn, m)
    };
    // Outside: the median of the outdoor rooms' air temperatures per day.
    let mut outside_days: Vec<BTreeMap<NaiveDate, f64>> = Vec::new();
    // Per room: each sensor's daily means, and the values standing now.
    let mut rooms: BTreeMap<String, (Vec<Days>, Vec<f64>)> = BTreeMap::new();
    for (id, m) in &all {
        if m.quantity != Quantity::Temperature || !air(m) {
            continue;
        }
        let room = m.room.clone().unwrap_or_else(|| m.name.clone());
        let is_outdoor = outdoor(&room);
        let wanted = meters.iter().any(|x| x.room == room);
        if !is_outdoor && !wanted {
            continue;
        }
        let p = points(*id)?;
        let days = daily_means(&p, from, now, HOLD, local_day);
        if is_outdoor {
            outside_days.push(days);
        } else {
            let r = rooms.entry(room).or_default();
            r.0.push(days);
            if let Some((t, v)) = p.last() {
                if now - t <= HOLD {
                    r.1.push(*v);
                }
            }
        }
    }
    let median_days = |maps: &[BTreeMap<NaiveDate, f64>]| -> BTreeMap<NaiveDate, f64> {
        let mut by: BTreeMap<NaiveDate, Vec<f64>> = BTreeMap::new();
        for m in maps {
            for (d, v) in m {
                by.entry(*d).or_default().push(*v);
            }
        }
        by.into_iter()
            .filter_map(|(d, v)| median(v).map(|m| (d, m)))
            .collect()
    };
    let has_outside = all.iter().any(|(_, m)| {
        m.quantity == Quantity::Temperature
            && air(m)
            && outdoor(m.room.as_deref().unwrap_or(&m.name))
    });
    Ok(Input {
        today: local_day(now),
        meters,
        outside: median_days(&outside_days),
        has_outside,
        room_now: rooms
            .iter()
            .filter_map(|(r, (_, now))| median(now.clone()).map(|v| (r.clone(), v)))
            .collect(),
        room_c: rooms
            .into_iter()
            .map(|(r, (days, _))| (r, median_days(&days)))
            .collect(),
    })
}

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called at startup when the wM-Bus source is configured: the page and its
/// link exist only then.
pub fn enable() {
    ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// GET /heizung
pub async fn heizung_page(State(state): State<AppState>) -> Response {
    let s = state.clone();
    let now = now_ms();
    match run_blocking(move || load(&s, now)).await {
        Ok(input) => page(
            "Heizung",
            &format!(r#"<div class="heizung">{}</div>"#, render(&input)),
        )
        .into_response(),
        Err(e) => {
            tracing::error!("heizung: {e}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "store unreadable",
            )
                .into_response()
        }
    }
}

fn units(v: f64) -> String {
    format!("{v:.0}")
}

/// The days before today in the last `n` days (today is still counting).
fn last_days(m: &BTreeMap<NaiveDate, f64>, today: NaiveDate, n: i64) -> f64 {
    let first = today - chrono::Duration::days(n);
    m.range(first..today).map(|(_, v)| v).sum()
}

fn fit_text(f: &Fit, per: &str, more: &str, less: &str) -> String {
    let dir = if f.slope >= 0.0 { more } else { less };
    format!(
        "{:.1} units/day {dir} per {per} (r = {:.2}, {} days)",
        f.slope.abs(),
        f.r,
        f.n
    )
}

/// The page body; pure, so the tests see exactly what is shown.
pub fn render(i: &Input) -> String {
    let mut h = String::from("<h1>Heizung</h1>");
    if i.meters.is_empty() {
        h.push_str(r#"<p class="empty">No heating cost allocator has sent anything yet.</p>"#);
        return h;
    }
    let total = sum_days(i.meters.iter().map(|m| &m.units));
    let past = |d: &BTreeMap<NaiveDate, f64>| -> BTreeMap<NaiveDate, f64> {
        d.range(..i.today).map(|(k, v)| (*k, *v)).collect()
    };
    let period: f64 = i.meters.iter().filter_map(|m| m.period).sum();
    let _ = write!(
        h,
        r#"<div class="facts"><span>Today <b>{}</b></span><span>Last 7 days <b>{}</b></span><span>This period <b>{}</b> units</span></div>"#,
        units(total.get(&i.today).copied().unwrap_or(0.0)),
        units(last_days(&total, i.today, 7)),
        units(period)
    );

    // Per meter.
    h.push_str(r#"<table class="heat"><thead><tr><th>Allocator</th><th>Room</th><th class="num">Today</th><th class="num">7 days</th><th class="num">30 days</th><th class="num">Period</th><th class="num">Share</th></tr></thead><tbody>"#);
    for (k, m) in i.meters.iter().enumerate() {
        let share = m
            .period
            .filter(|_| period > 0.0)
            .map(|p| format!("{:.0} %", p / period * 100.0))
            .unwrap_or("–".into());
        let _ = write!(
            h,
            r#"<tr><td><span class="sw m{}"></span>{}</td><td>{}</td><td class="num">{}</td><td class="num">{}</td><td class="num">{}</td><td class="num">{}</td><td class="num">{share}</td></tr>"#,
            k % 6,
            esc(&m.name),
            esc(&m.room),
            units(m.units.get(&i.today).copied().unwrap_or(0.0)),
            units(last_days(&m.units, i.today, 7)),
            units(last_days(&m.units, i.today, CHART_DAYS)),
            m.period.map(units).unwrap_or("–".into()),
        );
    }
    h.push_str("</tbody></table>");

    h.push_str("<h2>Units per day</h2>");
    h.push_str(&bars(i));

    // Usage against the outside temperature.
    h.push_str("<h2>Usage and the outside temperature</h2>");
    if !i.has_outside {
        h.push_str(r#"<p class="empty">No outside temperature: mark the outdoor room with <code>"outdoor": true</code> in the warning settings.</p>"#);
    } else {
        let pts: Vec<(f64, f64)> = pair(&i.outside, &past(&total))
            .into_iter()
            .map(|(_, x, y)| (x, y))
            .collect();
        match fit(&pts) {
            Some(f) => {
                let _ = write!(
                    h,
                    "<p>{}.</p>",
                    fit_text(&f, "°C colder outside", "less", "more")
                );
                h.push_str(&scatter(&pts, Some(f), "outside °C", "units/day"));
            }
            None => {
                let _ = write!(
                    h,
                    r#"<p class="empty">{} of the {MIN_DAYS} full days needed so far.</p>"#,
                    pts.len()
                );
            }
        }
    }

    // Usage against the room temperature, per room.
    h.push_str("<h2>Usage and the room temperature</h2>");
    let mut by_room: BTreeMap<&str, Vec<&Meter>> = BTreeMap::new();
    for m in &i.meters {
        by_room.entry(m.room.as_str()).or_default().push(m);
    }
    h.push_str(r#"<table><thead><tr><th>Room</th><th class="num">Room °C (7 d)</th><th>Relation</th></tr></thead><tbody>"#);
    for (room, ms) in &by_room {
        let used = past(&sum_days(ms.iter().map(|m| &m.units)));
        let empty = BTreeMap::new();
        let rc = i.room_c.get(*room).unwrap_or(&empty);
        let pts: Vec<(f64, f64)> = pair(rc, &used)
            .into_iter()
            .map(|(_, x, y)| (x, y))
            .collect();
        let week: Vec<f64> = rc
            .range(i.today - chrono::Duration::days(7)..i.today)
            .map(|(_, v)| *v)
            .collect();
        let mean = if week.is_empty() {
            "–".to_string()
        } else {
            format!("{:.1}", week.iter().sum::<f64>() / week.len() as f64)
        };
        let rel = if rc.is_empty() {
            "no own air sensor in this room".to_string()
        } else {
            match fit(&pts) {
                Some(f) => fit_text(&f, "°C warmer room", "more", "less"),
                None => format!("{} of the {MIN_DAYS} full days needed so far", pts.len()),
            }
        };
        let _ = write!(
            h,
            r#"<tr><td>{}</td><td class="num">{mean}</td><td>{}</td></tr>"#,
            esc(room),
            esc(&rel)
        );
    }
    h.push_str("</tbody></table>");

    // The allocator's estimate next to haus's sensor.
    h.push_str("<h2>Allocator and own sensor</h2>");
    h.push_str(r#"<table><thead><tr><th>Allocator</th><th class="num">Allocator est. °C</th><th class="num">Own sensor °C</th><th class="num">Difference</th><th class="num">Radiator °C</th></tr></thead><tbody>"#);
    let c = |v: Option<f64>| v.map(|v| format!("{v:.1}")).unwrap_or("–".into());
    for m in &i.meters {
        let own = i.room_now.get(&m.room).copied();
        let diff = match (m.est_c, own) {
            (Some(a), Some(b)) => format!("{:+.1}", a - b),
            _ => "–".into(),
        };
        let _ = write!(
            h,
            r#"<tr><td>{}</td><td class="num">{}</td><td class="num">{}</td><td class="num">{diff}</td><td class="num">{}</td></tr>"#,
            esc(&m.name),
            c(m.est_c),
            c(own),
            c(m.radiator_c)
        );
    }
    h.push_str("</tbody></table>");
    h.push_str(r#"<p class="note">Units are the allocators' own counts (Techem). A day's units count on the day the reading arrived; today is still counting and is left out of the relations.</p>"#);
    h
}

/// Stacked daily bars of the last CHART_DAYS days, with the outside
/// temperature as a line when there is one.
fn bars(i: &Input) -> String {
    let (w, ht, left, bottom, top) = (720.0, 220.0, 36.0, 20.0, 10.0);
    let days: Vec<NaiveDate> = (0..CHART_DAYS)
        .rev()
        .map(|k| i.today - chrono::Duration::days(k))
        .collect();
    let stacks: Vec<Vec<f64>> = days
        .iter()
        .map(|d| {
            i.meters
                .iter()
                .map(|m| m.units.get(d).copied().unwrap_or(0.0))
                .collect()
        })
        .collect();
    let max = stacks
        .iter()
        .map(|s| s.iter().sum::<f64>())
        .fold(0.0, f64::max)
        .max(1.0);
    let plot_h = ht - bottom - top;
    let bw = (w - left) / days.len() as f64;
    let mut s = format!(
        r#"<svg class="chart" viewBox="0 0 {w} {ht}" role="img" aria-label="Units per day, last {CHART_DAYS} days">"#
    );
    let _ = write!(
        s,
        r#"<line class="axis" x1="{left}" y1="{y}" x2="{w}" y2="{y}"/><text class="tick" x="2" y="{t}">{}</text><text class="tick" x="2" y="{y}">0</text>"#,
        units(max),
        y = ht - bottom,
        t = top + 10.0
    );
    for (k, (d, st)) in days.iter().zip(&stacks).enumerate() {
        let x = left + k as f64 * bw;
        let mut y = ht - bottom;
        for (j, v) in st.iter().enumerate() {
            if *v <= 0.0 {
                continue;
            }
            let bh = v / max * plot_h;
            y -= bh;
            let _ = write!(
                s,
                r#"<rect class="m{}" x="{:.1}" y="{y:.1}" width="{:.1}" height="{bh:.1}"><title>{} {}: {}</title></rect>"#,
                j % 6,
                x + 1.0,
                (bw - 2.0).max(1.0),
                d.format("%d.%m."),
                esc(&i.meters[j].name),
                units(*v)
            );
        }
        if k % 7 == 0 || k + 1 == days.len() {
            let _ = write!(
                s,
                r#"<text class="tick" x="{x:.1}" y="{}">{}</text>"#,
                ht - 4.0,
                d.format("%d.%m.")
            );
        }
    }
    // Outside temperature on its own scale.
    let temps: Vec<(usize, f64)> = days
        .iter()
        .enumerate()
        .filter_map(|(k, d)| i.outside.get(d).map(|v| (k, *v)))
        .collect();
    if temps.len() >= 2 {
        let lo = temps
            .iter()
            .map(|t| t.1)
            .fold(f64::INFINITY, f64::min)
            .floor()
            - 1.0;
        let hi = temps
            .iter()
            .map(|t| t.1)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            + 1.0;
        let y = |v: f64| top + (hi - v) / (hi - lo) * plot_h;
        let mut path = String::new();
        let mut prev: Option<usize> = None;
        for (k, v) in &temps {
            let cmd = if prev == Some(k.wrapping_sub(1)) {
                'L'
            } else {
                'M'
            };
            let _ = write!(
                path,
                "{cmd}{:.1},{:.1}",
                left + (*k as f64 + 0.5) * bw,
                y(*v)
            );
            prev = Some(*k);
        }
        let _ = write!(
            s,
            r#"<path class="line out" d="{path}"/><text class="tick" x="{w}" y="{}" text-anchor="end">{hi:.0} °C outside</text><text class="tick" x="{w}" y="{}" text-anchor="end">{lo:.0} °C</text>"#,
            top + 10.0,
            ht - bottom - 4.0
        );
    }
    s.push_str("</svg>");
    s
}

/// A scatter plot of (x, y) with the fitted line.
fn scatter(pts: &[(f64, f64)], f: Option<Fit>, xl: &str, yl: &str) -> String {
    let (w, ht, left, bottom, top, right) = (480.0, 260.0, 40.0, 30.0, 10.0, 10.0);
    let xs = pts.iter().map(|p| p.0);
    let (x0, x1) = (
        xs.clone().fold(f64::INFINITY, f64::min).floor() - 1.0,
        xs.fold(f64::NEG_INFINITY, f64::max).ceil() + 1.0,
    );
    let y1 = pts.iter().map(|p| p.1).fold(0.0, f64::max).max(1.0) * 1.1;
    let px = |x: f64| left + (x - x0) / (x1 - x0) * (w - left - right);
    let py = |y: f64| top + (y1 - y) / y1 * (ht - top - bottom);
    let mut s = format!(
        r#"<svg class="chart scatter" viewBox="0 0 {w} {ht}" role="img" aria-label="{} against {}">"#,
        esc(yl),
        esc(xl)
    );
    let _ = write!(
        s,
        r#"<line class="axis" x1="{left}" y1="{b}" x2="{}" y2="{b}"/><line class="axis" x1="{left}" y1="{top}" x2="{left}" y2="{b}"/><text class="tick" x="{left}" y="{}">{x0:.0}</text><text class="tick" x="{}" y="{}" text-anchor="end">{x1:.0} {}</text><text class="tick" x="2" y="{}">{:.0}</text><text class="tick" x="2" y="{b}">0</text><text class="tick" x="{}" y="{}">{}</text>"#,
        w - right,
        ht - 10.0,
        w - right,
        ht - 10.0,
        esc(xl),
        top + 10.0,
        y1,
        left + 4.0,
        top + 10.0,
        esc(yl),
        b = ht - bottom
    );
    if let Some(f) = f {
        let y = |x: f64| (f.intercept + f.slope * x).clamp(0.0, y1);
        let _ = write!(
            s,
            r#"<line class="fit" x1="{:.1}" y1="{:.1}" x2="{:.1}" y2="{:.1}"/>"#,
            px(x0),
            py(y(x0)),
            px(x1),
            py(y(x1))
        );
    }
    for (x, y) in pts {
        let _ = write!(
            s,
            r#"<circle class="dot" cx="{:.1}" cy="{:.1}" r="3.5"/>"#,
            px(*x),
            py(*y)
        );
    }
    s.push_str("</svg>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, n).unwrap()
    }

    fn input() -> Input {
        // Ten days: usage 40 - 2 * outside °C; today (11th) still counting.
        let outside: BTreeMap<_, _> = (1..=10).map(|k| (d(k), k as f64)).collect();
        let a: BTreeMap<_, _> = (1..=11).map(|k| (d(k), 20.0 - k as f64)).collect();
        let b: BTreeMap<_, _> = (1..=11).map(|k| (d(k), 20.0 - k as f64)).collect();
        Input {
            today: d(11),
            meters: vec![
                Meter {
                    name: "Fenster".into(),
                    room: "Diele".into(),
                    units: a,
                    period: Some(300.0),
                    est_c: Some(21.5),
                    radiator_c: Some(41.0),
                },
                Meter {
                    name: "Tür".into(),
                    room: "Diele".into(),
                    units: b,
                    period: Some(100.0),
                    est_c: None,
                    radiator_c: None,
                },
            ],
            outside,
            has_outside: true,
            room_c: [("Diele".to_string(), BTreeMap::new())].into(),
            room_now: [("Diele".to_string(), 20.0)].into(),
        }
    }

    // rivet: verifies REQ-HEAT-001
    #[test]
    fn the_page_shows_usage_and_how_it_follows_the_weather() {
        let h = render(&input());
        // Today 9 + 9; the 7 days before: (13..19 summed) twice.
        assert!(h.contains("Today <b>18</b>"), "{h}");
        let week: f64 = (4..=10).map(|k| 20.0 - k as f64).sum::<f64>() * 2.0;
        assert!(h.contains(&format!("Last 7 days <b>{week}</b>")));
        assert!(h.contains("This period <b>400</b>"));
        assert!(h.contains("75 %") && h.contains("25 %"));
        // Total = 40 - 2x: 2 units/day more per degree colder, r = -1.
        assert!(
            h.contains("2.0 units/day more per °C colder outside (r = -1.00, 10 days)"),
            "{h}"
        );
        assert!(h.contains(r#"class="chart scatter""#) && h.contains(r#"class="line out""#));
        // No room sensor history yet; allocator vs own sensor: 21.5 vs 20.0.
        assert!(h.contains("no own air sensor in this room"));
        assert!(h.contains("+1.5") && h.contains("41.0"));
    }

    // rivet: verifies REQ-HEAT-001
    #[test]
    fn missing_data_is_said_not_drawn() {
        let mut i = input();
        i.has_outside = false;
        assert!(render(&i).contains(r#""outdoor": true"#));
        i.has_outside = true;
        i.outside = i.outside.into_iter().take(3).collect();
        assert!(render(&i).contains("3 of the 7 full days needed so far"));
        assert!(render(&Input::default()).contains("No heating cost allocator"));
        // Names are escaped.
        let mut i = input();
        i.meters[0].name = "<b>".into();
        assert!(render(&i).contains("&lt;b&gt;") && !render(&i).contains("<b><"));
    }
}
