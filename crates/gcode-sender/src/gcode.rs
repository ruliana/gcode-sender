//! A small G-code interpreter. It tracks the modal state a program leaves
//! behind and the machine-space extent of every motion, which is enough to
//! rebuild state when a job is resumed and to check a program against the
//! machine's boundaries before any of it is sent.
//!
//! It understands what CAM output for routers uses: units, distance mode,
//! planes, G0/G1/G2/G3/G38.x, G53, and the G54 work offset. Anything that
//! changes where coordinates land in ways it cannot follow is reported as
//! unsupported rather than guessed at.

use std::f64::consts::{FRAC_PI_2, TAU};

const MM_PER_INCH: f64 = 25.4;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Plane {
    Xy,
    Xz,
    Yz,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    Rapid,
    Linear,
    ArcCw,
    ArcCcw,
    Probe,
}

/// The modal state a controller must be in for a program to continue from
/// a given line.
#[derive(Clone, Debug, PartialEq)]
pub struct Modal {
    pub inches: bool,
    pub absolute: bool,
    pub plane: Plane,
    pub wcs: u32,
    pub inverse_time: bool,
    pub spindle: Option<char>,
    pub spindle_speed: Option<String>,
    pub coolant: Option<char>,
    pub feed: Option<String>,
}

impl Default for Modal {
    fn default() -> Self {
        Self {
            inches: false,
            absolute: true,
            plane: Plane::Xy,
            wcs: 54,
            inverse_time: false,
            spindle: None,
            spindle_speed: None,
            coolant: None,
            feed: None,
        }
    }
}

impl Modal {
    /// Units, distance mode, plane, work offset and feed mode on one line.
    pub fn state_line(&self) -> String {
        format!(
            "{} {} {} G{} {}",
            if self.inches { "G20" } else { "G21" },
            if self.absolute { "G90" } else { "G91" },
            match self.plane {
                Plane::Xy => "G17",
                Plane::Xz => "G18",
                Plane::Yz => "G19",
            },
            self.wcs,
            if self.inverse_time { "G93" } else { "G94" },
        )
    }

    /// Coolant, then spindle, if they are on.
    pub fn tool_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(m) = self.coolant {
            lines.push(format!("M{m}"));
        }
        if let Some(m) = self.spindle {
            match &self.spindle_speed {
                Some(s) => lines.push(format!("M{m} S{s}")),
                None => lines.push(format!("M{m}")),
            }
        }
        lines
    }

    pub fn feed_line(&self) -> Option<String> {
        self.feed.as_ref().map(|f| format!("F{f}"))
    }

    /// The feed rate in millimetres per minute, if it is a plain rate.
    pub fn feed_mm_per_min(&self) -> Option<f64> {
        if self.inverse_time {
            return None;
        }
        let feed: f64 = self.feed.as_ref()?.parse().ok()?;
        Some(if self.inches {
            feed * MM_PER_INCH
        } else {
            feed
        })
    }

    /// The lines that put a controller into this state.
    pub fn preamble(&self) -> Vec<String> {
        let mut lines = vec![self.state_line()];
        lines.extend(self.tool_lines());
        lines.extend(self.feed_line());
        lines
    }
}

/// A word the interpreter will not follow, with why.
#[derive(Debug, PartialEq)]
pub struct Unsupported(pub String);

#[derive(Clone, Debug, PartialEq)]
pub struct Extent {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Extent {
    fn point(p: [f64; 3]) -> Self {
        Self { min: p, max: p }
    }
}

pub struct Interpreter {
    pub modal: Modal,
    motion: Motion,
    /// Position in machine coordinates, in millimetres.
    pub position: [f64; 3],
    /// Where G54 zero is in machine coordinates, in millimetres.
    offset: [f64; 3],
}

struct Word {
    letter: char,
    value: f64,
    text: String,
}

fn words(line: &str) -> Result<Vec<Word>, Unsupported> {
    let mut out = Vec::new();
    let mut chars = line.chars().filter(|c| !c.is_whitespace()).peekable();
    while let Some(letter) = chars.next() {
        let letter = letter.to_ascii_uppercase();
        if letter == '[' || letter == '#' {
            return Err(Unsupported("parameters and expressions".into()));
        }
        if !letter.is_ascii_alphabetic() {
            return Err(Unsupported(format!("unexpected '{letter}'")));
        }
        let mut text = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() || matches!(c, '.' | '-' | '+') {
                text.push(c);
                chars.next();
            } else {
                break;
            }
        }
        let value = text
            .parse()
            .map_err(|_| Unsupported(format!("bad number after {letter}")))?;
        out.push(Word {
            letter,
            value,
            text,
        });
    }
    Ok(out)
}

impl Interpreter {
    pub fn new(position: [f64; 3], offset: [f64; 3]) -> Self {
        Self {
            modal: Modal::default(),
            motion: Motion::Rapid,
            position,
            offset,
        }
    }

    fn unit(&self) -> f64 {
        if self.modal.inches { MM_PER_INCH } else { 1.0 }
    }

    /// Runs one line and returns the extent of the motion it causes, if any.
    pub fn step(&mut self, line: &str) -> Result<Option<Extent>, Unsupported> {
        let words = words(line)?;
        let mut axes: [Option<f64>; 3] = [None; 3];
        let mut offsets: [Option<f64>; 3] = [None; 3];
        let mut radius = None;
        let mut machine_coords = false;
        let mut motion = None;
        for w in &words {
            let g = (w.value * 10.0).round() as i64;
            match w.letter {
                'G' => match g {
                    0 => motion = Some(Motion::Rapid),
                    10 => motion = Some(Motion::Linear),
                    20 => motion = Some(Motion::ArcCw),
                    30 => motion = Some(Motion::ArcCcw),
                    382..=385 => motion = Some(Motion::Probe),
                    170 => self.modal.plane = Plane::Xy,
                    180 => self.modal.plane = Plane::Xz,
                    190 => self.modal.plane = Plane::Yz,
                    200 => self.modal.inches = true,
                    210 => self.modal.inches = false,
                    530 => machine_coords = true,
                    540 => self.modal.wcs = 54,
                    900 => self.modal.absolute = true,
                    910 => self.modal.absolute = false,
                    930 => self.modal.inverse_time = true,
                    940 => self.modal.inverse_time = false,
                    280 | 281 | 300 | 301 | 100 | 920 | 921 | 922 | 923 | 550 | 560 | 570 | 580
                    | 590 | 591 | 592 | 593 => {
                        return Err(Unsupported(format!("G{}", w.text)));
                    }
                    _ => {}
                },
                'M' => match w.value as i64 {
                    3..=5 => {
                        self.modal.spindle =
                            (w.value as i64 != 5).then(|| char::from(b'0' + w.value as u8));
                        if self.modal.spindle.is_none() {
                            self.modal.spindle_speed = None;
                        }
                    }
                    7 | 8 => self.modal.coolant = Some(char::from(b'0' + w.value as u8)),
                    9 => self.modal.coolant = None,
                    _ => {}
                },
                'S' => self.modal.spindle_speed = Some(w.text.clone()),
                'F' => self.modal.feed = Some(w.text.clone()),
                'X' => axes[0] = Some(w.value),
                'Y' => axes[1] = Some(w.value),
                'Z' => axes[2] = Some(w.value),
                'I' => offsets[0] = Some(w.value),
                'J' => offsets[1] = Some(w.value),
                'K' => offsets[2] = Some(w.value),
                'R' => radius = Some(w.value),
                _ => {}
            }
        }
        if let Some(m) = motion {
            self.motion = m;
        }
        if axes.iter().all(Option::is_none) {
            return Ok(None);
        }

        let unit = self.unit();
        let start = self.position;
        let mut end = start;
        for i in 0..3 {
            if let Some(v) = axes[i] {
                end[i] = if machine_coords {
                    v * unit
                } else if self.modal.absolute {
                    v * unit + self.offset[i]
                } else {
                    start[i] + v * unit
                };
            }
        }
        self.position = end;

        let mut extent = Extent::point(start);
        extend(&mut extent, end);
        if matches!(self.motion, Motion::ArcCw | Motion::ArcCcw) {
            let clockwise = self.motion == Motion::ArcCw;
            let scaled = offsets.map(|o| o.map(|v| v * unit));
            arc_extent(
                &mut extent,
                self.modal.plane,
                start,
                end,
                scaled,
                radius.map(|r| r * unit),
                clockwise,
            )?;
        }
        Ok(Some(extent))
    }
}

fn extend(extent: &mut Extent, p: [f64; 3]) {
    for (i, v) in p.into_iter().enumerate() {
        extent.min[i] = extent.min[i].min(v);
        extent.max[i] = extent.max[i].max(v);
    }
}

/// Grows `extent` to cover an arc from `start` to `end` in `plane`.
fn arc_extent(
    extent: &mut Extent,
    plane: Plane,
    start: [f64; 3],
    end: [f64; 3],
    offsets: [Option<f64>; 3],
    radius: Option<f64>,
    clockwise: bool,
) -> Result<(), Unsupported> {
    let (a, b) = match plane {
        Plane::Xy => (0, 1),
        Plane::Xz => (0, 2),
        Plane::Yz => (1, 2),
    };
    let (sa, sb) = (start[a], start[b]);
    let (ea, eb) = (end[a], end[b]);
    let (ca, cb) = if let Some(r) = radius {
        // Radius form: the center lies on the perpendicular bisector. A
        // positive radius picks the shorter arc, a negative one the longer.
        let (da, db) = (ea - sa, eb - sb);
        let chord = da.hypot(db);
        if chord == 0.0 {
            return Err(Unsupported("a full circle given by radius".into()));
        }
        let h2 = r * r - chord * chord / 4.0;
        if h2 < -1e-9 {
            return Err(Unsupported("an arc radius too small for its chord".into()));
        }
        let h = h2.max(0.0).sqrt();
        let sign = if clockwise == (r > 0.0) { -1.0 } else { 1.0 };
        let (mid_a, mid_b) = (sa + da / 2.0, sb + db / 2.0);
        (
            mid_a + sign * h * (-db / chord),
            mid_b + sign * h * (da / chord),
        )
    } else {
        let (oa, ob) = match plane {
            Plane::Xy => (offsets[0], offsets[1]),
            Plane::Xz => (offsets[0], offsets[2]),
            Plane::Yz => (offsets[1], offsets[2]),
        };
        (sa + oa.unwrap_or(0.0), sb + ob.unwrap_or(0.0))
    };
    let r = (sa - ca).hypot(sb - cb);
    let start_angle = (sb - cb).atan2(sa - ca);
    let end_angle = (eb - cb).atan2(ea - ca);
    let mut sweep = if clockwise {
        start_angle - end_angle
    } else {
        end_angle - start_angle
    };
    while sweep <= 1e-9 {
        sweep += TAU;
    }
    // Extremes of a circle sit at the axis crossings the sweep passes.
    for quarter in 0..4 {
        let angle = quarter as f64 * FRAC_PI_2;
        let mut from_start = if clockwise {
            start_angle - angle
        } else {
            angle - start_angle
        };
        while from_start < 0.0 {
            from_start += TAU;
        }
        if from_start < sweep {
            let (pa, pb) = (ca + r * angle.cos(), cb + r * angle.sin());
            extent.min[a] = extent.min[a].min(pa);
            extent.max[a] = extent.max[a].max(pa);
            extent.min[b] = extent.min[b].min(pb);
            extent.max[b] = extent.max[b].max(pb);
        }
    }
    Ok(())
}
