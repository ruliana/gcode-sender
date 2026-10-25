//! The work coordinate systems, G54 to G59: which one is active, and where
//! each one's zero is in machine coordinates.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The systems a Grbl controller keeps, in the order `$#` reports them.
/// G59.1 to G59.3 are not here: Grbl 1.1 refuses them.
pub const SYSTEMS: [u32; 6] = [54, 55, 56, 57, 58, 59];

/// Zeros in machine coordinates, in millimetres, in the order of [`SYSTEMS`].
pub type Zeros = [[f64; 3]; 6];

/// Position of a system in [`SYSTEMS`], for a system number such as 55.
pub fn index(system: u32) -> Option<usize> {
    SYSTEMS.iter().position(|&s| s == system)
}

/// Reads a system name such as `G55`, `g55` or `55`.
pub fn parse_system(name: &str) -> Result<u32, String> {
    let digits = name.trim().trim_start_matches(['G', 'g']);
    digits
        .parse::<u32>()
        .ok()
        .filter(|s| index(*s).is_some())
        .ok_or_else(|| format!("system must be one of G54 to G59, not {name:?}"))
}

/// The active work coordinate system and where every system's zero is.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkOffsets {
    /// The active system, 54 to 59.
    pub system: u32,
    pub zeros: Zeros,
}

impl WorkOffsets {
    /// The zero of `system` in machine coordinates.
    pub fn zero(&self, system: u32) -> Option<[f64; 3]> {
        index(system).map(|i| self.zeros[i])
    }

    /// Each system's zero by name, as `GET /machine` reports it.
    fn by_name(&self) -> BTreeMap<String, [f64; 3]> {
        SYSTEMS
            .iter()
            .zip(self.zeros)
            .map(|(s, z)| (format!("G{s}"), z))
            .collect()
    }
}

/// The JSON shape: `{"system": "G54", "offsets": {"G54": [x, y, z], ...}}`.
#[derive(Serialize, Deserialize)]
struct Wire {
    system: String,
    offsets: BTreeMap<String, [f64; 3]>,
}

impl From<WorkOffsets> for Wire {
    fn from(w: WorkOffsets) -> Self {
        Self {
            system: format!("G{}", w.system),
            offsets: w.by_name(),
        }
    }
}

impl TryFrom<Wire> for WorkOffsets {
    type Error = String;

    fn try_from(w: Wire) -> Result<Self, Self::Error> {
        let system = parse_system(&w.system)?;
        let mut zeros = [[0.0; 3]; 6];
        for (name, zero) in &w.offsets {
            let s = parse_system(name)?;
            zeros[index(s).unwrap_or_default()] = *zero;
        }
        Ok(Self { system, zeros })
    }
}

impl Serialize for WorkOffsets {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Wire::from(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WorkOffsets {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_names_parse() {
        assert_eq!(parse_system("G54"), Ok(54));
        assert_eq!(parse_system("g59"), Ok(59));
        assert_eq!(parse_system("57"), Ok(57));
        assert!(parse_system("G53").is_err());
        assert!(parse_system("G59.1").is_err());
        assert!(parse_system("G60").is_err());
    }

    #[test]
    fn offsets_round_trip_through_json() {
        let mut zeros = [[0.0; 3]; 6];
        zeros[1] = [1.0, 2.0, 3.0];
        let work = WorkOffsets { system: 55, zeros };
        let json = serde_json::to_value(&work).unwrap();
        assert_eq!(json["system"], "G55");
        assert_eq!(json["offsets"]["G55"], serde_json::json!([1.0, 2.0, 3.0]));
        let back: WorkOffsets = serde_json::from_value(json).unwrap();
        assert_eq!(back, work);
    }
}
