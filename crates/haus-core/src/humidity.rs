//! Dew point and whether airing a room dries it (REQ-ANA-001, DD-027).
//!
//! The dew point measures how much water the air holds, whatever its
//! temperature. Outside air with a lower dew point than the room's carries
//! less water in: airing dries. Outside air whose dew point reaches the
//! room's temperature condenses on the room's walls, which are colder than
//! its air: the case of a cool cellar on a warm, humid day.

/// The dew point in °C of air at `t` °C and `rh` % relative humidity
/// (Magnus formula, Sonntag 1990 constants; within 0.1 K from -45 to 60 °C).
pub fn dew_point(t: f64, rh: f64) -> Option<f64> {
    if !(t.is_finite() && rh.is_finite()) || rh <= 0.0 || rh > 100.0 {
        return None;
    }
    const A: f64 = 17.62;
    const B: f64 = 243.12;
    let g = (rh / 100.0).ln() + A * t / (B + t);
    Some(B * g / (A - g))
}

/// What opening the windows would do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Airing {
    /// The outside air holds clearly less water.
    Dries,
    /// About the same water inside and out.
    NoDifference,
    /// The outside air holds clearly more water.
    Damper,
    /// The outside air's dew point reaches the room's temperature: water
    /// would condense on the colder walls.
    Condenses,
}

impl Airing {
    pub fn text(self) -> &'static str {
        match self {
            Airing::Dries => "airing dries",
            Airing::NoDifference => "airing changes little",
            Airing::Damper => "airing makes it damper",
            Airing::Condenses => "keep closed: outside air would condense",
        }
    }
}

/// How far apart the dew points must be to call it, in K: the sensors'
/// own error (about ±0.3 K and ±3 % RH) moves a dew point by about 0.6 K.
pub const MARGIN_K: f64 = 1.0;

/// The airing hint for a room at `room_c` with dew point `room_dp`, given
/// the outside dew point.
pub fn airing(room_c: f64, room_dp: f64, outside_dp: f64) -> Airing {
    if outside_dp >= room_c - MARGIN_K {
        Airing::Condenses
    } else if outside_dp <= room_dp - MARGIN_K {
        Airing::Dries
    } else if outside_dp >= room_dp + MARGIN_K {
        Airing::Damper
    } else {
        Airing::NoDifference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // rivet: verifies REQ-ANA-001
    #[test]
    fn dew_points_match_reference_values() {
        // Reference values (psychrometric tables): 20 °C/50 % → 9.3 °C,
        // 25 °C/80 % → 21.3 °C, 0 °C/100 % → 0 °C, -10 °C/70 % → -14.4 °C.
        for (t, rh, want) in [
            (20.0, 50.0, 9.3),
            (25.0, 80.0, 21.3),
            (0.0, 100.0, 0.0),
            (-10.0, 70.0, -14.4),
        ] {
            let got = dew_point(t, rh).unwrap();
            assert!((got - want).abs() < 0.15, "{t} °C {rh} %: {got}");
        }
        for (t, rh) in [(20.0, 0.0), (20.0, 101.0), (f64::NAN, 50.0)] {
            assert!(dew_point(t, rh).is_none());
        }
    }

    // rivet: verifies DD-027
    // rivet: verifies REQ-ANA-001
    #[test]
    fn airing_dries_a_humid_bathroom_but_not_a_cool_cellar_in_summer() {
        // Bathroom 22 °C, dew point 15.8; winter outside dew point 2.
        assert_eq!(airing(22.0, 15.8, 2.0), Airing::Dries);
        // Cellar 14 °C, dew point 10.4; humid summer air, dew point 16.
        assert_eq!(airing(14.0, 10.4, 16.0), Airing::Condenses);
        // Outside clearly wetter, but below the room's temperature.
        assert_eq!(airing(20.0, 9.0, 12.0), Airing::Damper);
        // Within the margin either way.
        assert_eq!(airing(20.0, 9.0, 9.5), Airing::NoDifference);
        assert_eq!(airing(20.0, 9.0, 8.2), Airing::NoDifference);
        // The boundaries: exactly one margin apart decides.
        assert_eq!(airing(20.0, 9.0, 8.0), Airing::Dries);
        assert_eq!(airing(20.0, 9.0, 10.0), Airing::Damper);
        assert_eq!(airing(20.0, 9.0, 19.0), Airing::Condenses);
    }
}
