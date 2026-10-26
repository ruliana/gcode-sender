//! Settings that outlive jobs and server restarts, kept in
//! `<data>/settings.json`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub pause: PauseSettings,
    pub position: PositionSettings,
    pub jog: JogSettings,
}

/// The largest feed a jog may be saved with, in millimetres per minute.
pub const MAX_JOG_FEED: f64 = 10_000.0;

/// How fast jogs by hand go, in millimetres per minute. X and Y share one
/// speed, and Z has its own, usually slower, as on most CNC software.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JogSettings {
    /// Feed for jogs along X or Y.
    pub xy_feed: f64,
    /// Feed for jogs along Z.
    pub z_feed: f64,
}

impl Default for JogSettings {
    fn default() -> Self {
        Self {
            xy_feed: 1000.0,
            z_feed: 300.0,
        }
    }
}

impl JogSettings {
    pub fn validate(&self) -> Result<(), String> {
        for (name, feed) in [("jog.xy_feed", self.xy_feed), ("jog.z_feed", self.z_feed)] {
            if !(feed.is_finite() && feed > 0.0 && feed <= MAX_JOG_FEED) {
                return Err(format!(
                    "{name} must be above zero and at most {MAX_JOG_FEED} millimetres per minute"
                ));
            }
        }
        Ok(())
    }

    /// The feed for a jog along the axes given as `[x, y, z]`, where `true`
    /// means the jog moves that axis. A move that uses both speeds runs at
    /// the slower one, so no axis goes faster than its setting.
    pub fn feed_for(&self, axes: [bool; 3]) -> f64 {
        let xy = axes[0] || axes[1];
        let z = axes[2];
        match (xy, z) {
            (true, true) => self.xy_feed.min(self.z_feed),
            (true, false) => self.xy_feed,
            _ => self.z_feed,
        }
    }
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
        self.pause.validate()?;
        self.jog.validate()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mixed_move_runs_at_the_slower_speed() {
        let jog = JogSettings {
            xy_feed: 800.0,
            z_feed: 150.0,
        };
        assert_eq!(jog.feed_for([true, false, false]), 800.0);
        assert_eq!(jog.feed_for([false, true, false]), 800.0);
        assert_eq!(jog.feed_for([false, false, true]), 150.0);
        assert_eq!(jog.feed_for([true, false, true]), 150.0);
        let fast_z = JogSettings {
            xy_feed: 800.0,
            z_feed: 2000.0,
        };
        assert_eq!(fast_z.feed_for([true, true, true]), 800.0);
    }
}
