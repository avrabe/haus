//! Wireless M-Bus meters read by wmbusmeters (REQ-WMBUS-001, DD-025).
//!
//! wmbusmeters writes one JSON object per decoded telegram. A radio stick
//! hears every meter in the building, so haus keeps only the meters on the
//! owner's private list (`HAUS_WMBUS_METERS`): a line of any other meter is
//! dropped unread and never logged, not even its id.
//!
//! For a heating cost allocator (`fhkvdataiii`) haus keeps the allocator's
//! units, the room temperature it estimates and the radiator temperature.
//! The series names are built here, so the radiator temperature always
//! counts as a radiator probe and the room temperature as room air.

use crate::ingest::Item;
use crate::store::{Quantity, SeriesKey, SeriesMeta, Source};
use serde::Deserialize;
use std::collections::HashMap;

/// One of the owner's meters, from the private list.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meter {
    /// What the meter is, e.g. "Bad" or "Wohnzimmer Fenster".
    pub name: String,
    /// The haus room its values belong to.
    pub room: String,
}

/// The owner's meters by wM-Bus id (8 hex digits, as wmbusmeters prints it).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meters(HashMap<String, Meter>);

/// Words that mark a series as not room air (see warn::WarnConfig::not_air);
/// a meter's own name must not carry one, or its room temperature would be
/// taken for a probe.
const NOT_AIR_WORDS: [&str; 6] = [
    "Heizkörper",
    "Radiator",
    "Fußboden",
    "Floor",
    "Stellantrieb",
    "FBH",
];

impl Meters {
    /// Parse and check the private list; errors name the id, never echo
    /// another meter's data.
    pub fn parse(json: &str) -> Result<Meters, String> {
        let map: HashMap<String, Meter> =
            serde_json::from_str(json).map_err(|e| format!("meter list: {e}"))?;
        if map.is_empty() {
            return Err("meter list: no meters".into());
        }
        let mut out = HashMap::new();
        for (id, m) in map {
            if id.len() != 8 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("{id:?}: an id is 8 hex digits"));
            }
            for (field, v) in [("name", &m.name), ("room", &m.room)] {
                let n = v.trim().chars().count();
                if !(1..=40).contains(&n) {
                    return Err(format!("{id}.{field}: 1 to 40 characters, got {n}"));
                }
            }
            if let Some(w) = NOT_AIR_WORDS.iter().find(|w| m.name.contains(**w)) {
                return Err(format!(
                    "{id}.name: must not contain {w:?} (haus names the radiator series itself)"
                ));
            }
            out.insert(
                id.to_ascii_lowercase(),
                Meter {
                    name: m.name.trim().to_string(),
                    room: m.room.trim().to_string(),
                },
            );
        }
        Ok(Meters(out))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn get(&self, id: &str) -> Option<&Meter> {
        self.0.get(&id.to_ascii_lowercase())
    }
}

/// The fields haus keeps of a heating cost allocator telegram.
#[derive(Deserialize)]
struct Allocator {
    id: String,
    timestamp: String,
    #[serde(default)]
    current_hca: Option<f64>,
    #[serde(default)]
    temp_room_c: Option<f64>,
    #[serde(default)]
    temp_radiator_c: Option<f64>,
}

/// The items of one log line: empty for a line that is not JSON (raw
/// telegram lines), not an allocator haus reads, or not on the owner's
/// list. Never an error: the log is someone else's output.
pub fn parse_line(line: &str, meters: &Meters) -> Vec<Item> {
    let line = line.trim();
    if !line.starts_with('{') {
        return vec![];
    }
    // The id first, so a foreign meter's line is not parsed any further.
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return vec![];
    };
    let Some(meter) = v["id"].as_str().and_then(|id| meters.get(id)) else {
        return vec![];
    };
    if v["driver"].as_str() != Some("fhkvdataiii") {
        return vec![];
    }
    let Ok(a) = serde_json::from_value::<Allocator>(v) else {
        return vec![];
    };
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&a.timestamp) else {
        return vec![];
    };
    let ts_ms = ts.timestamp_millis();
    let id = a.id.to_ascii_lowercase();
    let item = |field: &str, quantity, unit: &str, name: String, value: Option<f64>| {
        value.filter(|v| v.is_finite()).map(|value| Item {
            meta: SeriesMeta {
                key: SeriesKey::new(Source::Wmbus, id.clone(), field),
                quantity,
                unit: unit.into(),
                name,
                room: Some(meter.room.clone()),
            },
            ts_ms,
            value,
        })
    };
    [
        item(
            "current_hca",
            Quantity::Other,
            "",
            format!("Heizkostenverteiler {}", meter.name),
            a.current_hca,
        ),
        item(
            "temp_room_c",
            Quantity::Temperature,
            "°C",
            format!("Temperatur {}", meter.name),
            a.temp_room_c,
        ),
        item(
            "temp_radiator_c",
            Quantity::Temperature,
            "°C",
            format!("Heizkörper {}", meter.name),
            a.temp_radiator_c,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::warn::{is_air, WarnConfig};

    const MINE: &str = r#"{"0000000a": {"name": "Fenster", "room": "Diele"}}"#;

    fn line(id: &str, driver: &str) -> String {
        format!(
            r#"{{"_":"telegram","media":"heat cost allocator","driver":"{driver}","name":"x","id":"{id}","current_date":"2026-11-01","current_hca":37,"previous_date":"2025-12-31","previous_hca":412,"temp_radiator_c":41.5,"temp_room_c":20.25,"timestamp":"2026-11-17T08:15:00Z","device":"iu891a[0]","rssi_dbm":-71}}"#
        )
    }

    // rivet: verifies DD-010
    // rivet: verifies DD-025
    // rivet: verifies REQ-WMBUS-001
    #[test]
    fn only_the_owners_allocators_become_series() {
        let m = Meters::parse(MINE).unwrap();
        let items = parse_line(&line("0000000A", "fhkvdataiii"), &m);
        let got: Vec<(&str, f64, &str)> = items
            .iter()
            .map(|i| (i.meta.key.field.as_str(), i.value, i.meta.name.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("current_hca", 37.0, "Heizkostenverteiler Fenster"),
                ("temp_room_c", 20.25, "Temperatur Fenster"),
                ("temp_radiator_c", 41.5, "Heizkörper Fenster"),
            ]
        );
        let i = &items[0];
        assert_eq!(
            (i.meta.key.source, i.meta.key.source_id.as_str()),
            (Source::Wmbus, "0000000a")
        );
        assert_eq!(i.meta.room.as_deref(), Some("Diele"));
        assert_eq!(i.ts_ms, 1_794_903_300_000);
        // The room temperature is room air, the radiator temperature is not.
        let cfg = WarnConfig::default();
        assert!(is_air(&cfg, &items[1].meta));
        assert!(!is_air(&cfg, &items[2].meta));
        // A neighbour's meter, another driver, raw telegram lines, junk: nothing.
        assert!(parse_line(&line("0000000b", "fhkvdataiii"), &m).is_empty());
        assert!(parse_line(&line("0000000a", "vario451mid"), &m).is_empty());
        assert!(parse_line("telegram=|5E44A2...|+71", &m).is_empty());
        assert!(parse_line("{not json", &m).is_empty());
        assert!(parse_line(
            &line("0000000a", "fhkvdataiii").replace("2026-11-17T08:15:00Z", "soon"),
            &m
        )
        .is_empty());
    }

    // rivet: verifies DD-025
    // rivet: verifies REQ-WMBUS-001
    #[test]
    fn a_bad_meter_list_is_refused_naming_the_id() {
        for (json, names) in [
            (r#"{}"#, "no meters"),
            (r#"{"12345": {"name": "a", "room": "b"}}"#, "\"12345\""),
            (
                r#"{"0000000g": {"name": "a", "room": "b"}}"#,
                "\"0000000g\"",
            ),
            (
                r#"{"0000000a": {"name": " ", "room": "b"}}"#,
                "0000000a.name",
            ),
            (
                r#"{"0000000a": {"name": "a", "room": ""}}"#,
                "0000000a.room",
            ),
            (
                r#"{"0000000a": {"name": "Heizkörper Bad", "room": "b"}}"#,
                "0000000a.name",
            ),
            (
                r#"{"0000000a": {"name": "a", "room": "b", "key": "x"}}"#,
                "meter list",
            ),
            (r#"["0000000a"]"#, "meter list"),
        ] {
            let e = Meters::parse(json).unwrap_err();
            assert!(e.contains(names), "{json}: {e}");
        }
        assert_eq!(Meters::parse(MINE).unwrap().len(), 1);
    }
}
