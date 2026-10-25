//! Settings that outlive jobs and server restarts, kept in
//! `<data>/settings.json`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub pause: PauseSettings,
    pub position: PositionSettings,
}

/// What to do when the controller reports a different position than the
/// sender last recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PositionSettings {
    /// Trust the recorded position: keep the tool where the record puts it by
    /// moving the work zero, then ask the operator whether the machine moved.
    /// Off, jobs wait until the operator has answered.
    pub trust_record: bool,
}

impl Default for PositionSettings {
    fn default() -> Self {
        Self { trust_record: true }
    }
}

/// What pausing a job does to the tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PauseSettings {
    /// Lift the tool when pausing, free the machine for jogging, and bring
    /// the tool back to where it was when the job resumes. Off, a pause is a
    /// plain feed hold that leaves the tool where it is.
    pub lift: bool,
    /// How far to lift, in millimetres, never past the Z boundary.
    pub height: f64,
    /// Lift to this Z in machine coordinates instead of by `height`.
    pub z: Option<f64>,
    /// Seconds to wait for the spindle to reach speed after it restarts on
    /// resume, if the program had it on.
    pub dwell_s: f64,
}

impl Default for PauseSettings {
    fn default() -> Self {
        Self {
            lift: true,
            height: 10.0,
            z: None,
            dwell_s: 3.0,
        }
    }
}

impl PauseSettings {
    pub fn validate(&self) -> Result<(), String> {
        if !(self.height.is_finite() && self.height > 0.0 && self.height <= 500.0) {
            return Err("pause.height must be between 0 and 500 millimetres".into());
        }
        if self.z.is_some_and(|z| !z.is_finite()) {
            return Err("pause.z must be a number".into());
        }
        if !(0.0..=60.0).contains(&self.dwell_s) {
            return Err("pause.dwell_s must be between 0 and 60 seconds".into());
        }
        Ok(())
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        self.pause.validate()
    }
}

/// Per-request overrides for `POST /jobs/{id}/pause`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PauseOptions {
    pub lift: Option<bool>,
    pub height: Option<f64>,
    pub z: Option<f64>,
}

impl PauseOptions {
    /// The saved settings with these overrides on top.
    pub fn apply(&self, saved: &PauseSettings) -> Result<PauseSettings, String> {
        let mut settings = saved.clone();
        if let Some(lift) = self.lift {
            settings.lift = lift;
        }
        if let Some(height) = self.height {
            settings.height = height;
        }
        if self.z.is_some() {
            settings.z = self.z;
        }
        settings.validate()?;
        Ok(settings)
    }
}
