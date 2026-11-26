//! Property tests for the G-code side: how a program is prepared for the
//! controller, where its moves land in machine coordinates, how far its arcs
//! reach, and whether it stays inside the boundaries.
//!
//! The arc checks compare the interpreter with arcs built from first
//! principles: sample points along the true circle and see whether the
//! interpreter's extent covers them, and no more than they need.

use gcode_sender::bounds::{self, Bounds};
use gcode_sender::gcode::{Interpreter, systems_selected};
use gcode_sender::program::{ProgramLine, prepare};
use gcode_sender::work::{SYSTEMS, WorkOffsets, Zeros};
use proptest::prelude::*;

/// Tolerance for the arc checks, in millimetres. Endpoints are printed with
/// four decimals, which moves a sampled point by a few hundredths of a
/// millionth of a millimetre, and the sampling itself is finer than this.
const ARC_TOL: f64 = 1e-2;

/// A value on a grid of hundredths, which prints and reads back exactly.
fn hundredths(range: std::ops::RangeInclusive<i64>) -> impl Strategy<Value = f64> {
    range.prop_map(|v| v as f64 / 100.0)
}

fn zeros_strategy() -> impl Strategy<Value = Zeros> {
    prop::collection::vec(hundredths(-100_000..=100_000), 18).prop_map(|v| {
        let mut zeros = [[0.0; 3]; 6];
        for (i, zero) in zeros.iter_mut().enumerate() {
            *zero = [v[3 * i], v[3 * i + 1], v[3 * i + 2]];
        }
        zeros
    })
}

fn source_text() -> impl Strategy<Value = String> {
    let alphabet = vec![
        'G', 'X', 'Y', 'Z', 'I', 'J', 'M', 'F', 'S', '0', '1', '2', '5', '.', '-', ' ', ';', '(',
        ')', '%', '[', '#', '\t', 'x', 'g',
    ];
    prop::collection::vec(
        prop::collection::vec(prop::sample::select(alphabet), 0..24),
        0..12,
    )
    .prop_map(|lines| {
        lines
            .into_iter()
            .map(|cs| cs.into_iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

proptest! {
    #[test]
    fn preparing_a_program_keeps_its_lines_in_order_and_adds_nothing(source in source_text()) {
        let lines = prepare(&source);
        let source_lines: Vec<&str> = source.lines().collect();
        let mut last = 0;
        for line in &lines {
            prop_assert!(line.number > last, "line numbers must increase");
            last = line.number;
            prop_assert!(line.number <= source_lines.len());
            prop_assert!(!line.text.is_empty());
            prop_assert_eq!(line.text.trim(), line.text.as_str());
            prop_assert!(!line.text.contains(';'));
            prop_assert!(line.text != "%");
            prop_assert!(
                is_subsequence(&line.text, source_lines[line.number - 1]),
                "line {} invents text: {:?} from {:?}",
                line.number,
                line.text,
                source_lines[line.number - 1]
            );
        }
    }

    #[test]
    fn preparing_a_prepared_program_changes_nothing(source in source_text()) {
        let once = prepare(&source);
        let text: String = once.iter().map(|l| format!("{}\n", l.text)).collect();
        let twice: Vec<String> = prepare(&text).into_iter().map(|l| l.text).collect();
        let once: Vec<String> = once.into_iter().map(|l| l.text).collect();
        prop_assert_eq!(twice, once);
    }

    #[test]
    fn the_interpreter_never_panics_on_arbitrary_lines(
        line in prop::collection::vec(any::<char>(), 0..80),
    ) {
        let line: String = line.into_iter().collect();
        let mut m = Interpreter::new([0.0; 3], [[0.0; 3]; 6], 54);
        let _ = m.step(&line);
    }

    #[test]
    fn absolute_moves_land_at_their_value_plus_the_active_zero(
        zeros in zeros_strategy(),
        system in prop::sample::select(SYSTEMS.to_vec()),
        value in hundredths(-100_000..=100_000),
        start in hundredths(-100_000..=100_000),
    ) {
        let mut m = Interpreter::new([start, 0.0, 0.0], zeros, system);
        m.step(&format!("G1 X{value:.2}")).unwrap();
        let zero = zeros[SYSTEMS.iter().position(|&s| s == system).unwrap()];
        prop_assert!((m.position[0] - (value + zero[0])).abs() < 1e-9);
        // Y was not named, so it stays where it started.
        prop_assert!(m.position[1].abs() < 1e-9);
    }

    #[test]
    fn a_work_system_named_on_the_line_is_the_one_used(
        zeros in zeros_strategy(),
        selected in prop::sample::select(SYSTEMS.to_vec()),
        active in prop::sample::select(SYSTEMS.to_vec()),
        value in hundredths(-50_000..=50_000),
    ) {
        let mut m = Interpreter::new([0.0; 3], zeros, active);
        m.step(&format!("G{selected} G1 Y{value:.2}")).unwrap();
        let zero = zeros[SYSTEMS.iter().position(|&s| s == selected).unwrap()];
        prop_assert!((m.position[1] - (value + zero[1])).abs() < 1e-9);
        prop_assert_eq!(m.modal.wcs, selected);
    }

    #[test]
    fn machine_coordinates_ignore_the_work_zero(
        zeros in zeros_strategy(),
        system in prop::sample::select(SYSTEMS.to_vec()),
        value in hundredths(-100_000..=100_000),
    ) {
        let mut m = Interpreter::new([0.0; 3], zeros, system);
        m.step(&format!("G53 G0 Z{value:.2}")).unwrap();
        prop_assert!((m.position[2] - value).abs() < 1e-9);
    }

    #[test]
    fn relative_moves_are_measured_from_where_the_tool_is(
        start in hundredths(-50_000..=50_000),
        step in hundredths(-50_000..=50_000),
    ) {
        let mut m = Interpreter::new([start, 0.0, 0.0], [[0.0; 3]; 6], 54);
        m.step("G91").unwrap();
        m.step(&format!("G1 X{step:.2}")).unwrap();
        prop_assert!((m.position[0] - (start + step)).abs() < 1e-9);
    }

    #[test]
    fn inch_programs_scale_moves_and_keep_the_zero_in_millimetres(
        zeros in zeros_strategy(),
        value in hundredths(-1_000..=1_000),
    ) {
        let mut m = Interpreter::new([0.0; 3], zeros, 54);
        m.step(&format!("G20 G1 X{value:.2}")).unwrap();
        prop_assert!((m.position[0] - (value * 25.4 + zeros[0][0])).abs() < 1e-6);
    }

    #[test]
    fn a_system_word_is_only_read_as_a_g5x_word(
        system in prop::sample::select(SYSTEMS.to_vec()),
        other in prop::sample::select(vec!["59.1", "59.2", "53", "28", "30", "92", "1.5", "0"]),
    ) {
        let line = format!("G{system} G{other} X1");
        prop_assert_eq!(systems_selected(&line), vec![system]);
    }

    /// Centre-form arcs, both directions, over every sweep from 1 to 359
    /// degrees. The extent must cover the true arc and must not be larger
    /// than it.
    #[test]
    fn centre_form_arc_extents_cover_the_arc_and_no_more(
        cx in hundredths(-20_000..=20_000),
        cy in hundredths(-20_000..=20_000),
        radius in hundredths(50..=20_000),
        start_deg in 0u32..360,
        sweep_deg in 1u32..360,
        clockwise in any::<bool>(),
    ) {
        let extent = arc(cx, cy, radius, start_deg, sweep_deg, clockwise);
        // Sample the true circle densely; every sample must be inside.
        let a0 = (start_deg as f64).to_radians();
        let dir = if clockwise { -1.0 } else { 1.0 };
        let samples = 1440;
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for k in 0..=samples {
            let a = a0 + dir * (sweep_deg as f64).to_radians() * k as f64 / samples as f64;
            let p = [cx + radius * a.cos(), cy + radius * a.sin()];
            for axis in 0..2 {
                lo[axis] = lo[axis].min(p[axis]);
                hi[axis] = hi[axis].max(p[axis]);
                prop_assert!(
                    p[axis] >= extent.min[axis] - ARC_TOL && p[axis] <= extent.max[axis] + ARC_TOL,
                    "sample {p:?} of the arc lies outside {extent:?}"
                );
            }
        }
        for axis in 0..2 {
            prop_assert!(extent.min[axis] <= lo[axis] + ARC_TOL, "extent too large: {extent:?}");
            prop_assert!(extent.max[axis] >= hi[axis] - ARC_TOL, "extent too small: {extent:?}");
            prop_assert!(extent.max[axis] <= hi[axis] + ARC_TOL, "extent too large: {extent:?}");
        }
    }

    /// Radius-form arcs: a positive radius is the shorter arc and a negative
    /// one the longer, in either direction. Both forms must describe the same
    /// arc.
    #[test]
    fn radius_form_arcs_match_the_centre_form(
        cx in hundredths(-5_000..=5_000),
        cy in hundredths(-5_000..=5_000),
        radius in hundredths(100..=5_000),
        start_deg in 0u32..360,
        sweep_deg in 1u32..360,
        clockwise in any::<bool>(),
    ) {
        prop_assume!(sweep_deg != 180);
        let centre = arc(cx, cy, radius, start_deg, sweep_deg, clockwise);
        let (ex, ey) = endpoint(cx, cy, radius, start_deg, sweep_deg, clockwise);
        let short = sweep_deg < 180;
        let r = if short { radius } else { -radius };
        let mut m = Interpreter::new(start_point(cx, cy, radius, start_deg), [[0.0; 3]; 6], 54);
        let g = if clockwise { "G2" } else { "G3" };
        let extent = m
            .step(&format!("{g} X{ex:.4} Y{ey:.4} R{r:.4}"))
            .unwrap()
            .unwrap();
        for axis in 0..2 {
            prop_assert!((extent.min[axis] - centre.min[axis]).abs() < ARC_TOL, "{extent:?} vs {centre:?}");
            prop_assert!((extent.max[axis] - centre.max[axis]).abs() < ARC_TOL, "{extent:?} vs {centre:?}");
        }
    }

    #[test]
    fn a_boundary_check_agrees_with_the_moves_it_checks(
        zero_x in hundredths(-50_000..=50_000),
        target in hundredths(-50_000..=50_000),
        low in hundredths(-50_000..=50_000),
        span in hundredths(0..=50_000),
    ) {
        let high = low + span;
        let mut zeros = [[0.0; 3]; 6];
        zeros[0][0] = zero_x;
        let work = WorkOffsets { system: 54, zeros };
        let limits = Bounds { x: Some([low, high]), y: None, z: None };
        let lines = vec![ProgramLine { number: 1, text: format!("G1 X{target:.2}") }];
        // The move starts where the tool is, which must be inside the limits.
        let start = low;
        let machine = target + zero_x;
        let inside = machine >= low - 1e-6 && machine <= high + 1e-6;
        let verdict = bounds::check(&limits, &lines, [start, 0.0, 0.0], &work);
        prop_assert!(
            verdict.is_ok() == inside,
            "machine x {machine} against [{low}, {high}]: {verdict:?}"
        );
    }
}

fn start_point(cx: f64, cy: f64, radius: f64, start_deg: u32) -> [f64; 3] {
    let a = (start_deg as f64).to_radians();
    [
        round4(cx + radius * a.cos()),
        round4(cy + radius * a.sin()),
        0.0,
    ]
}

fn round4(v: f64) -> f64 {
    (v * 1e4).round() / 1e4
}

/// The endpoint of an arc, printed to four decimals as a program would say it.
fn endpoint(
    cx: f64,
    cy: f64,
    radius: f64,
    start_deg: u32,
    sweep_deg: u32,
    clockwise: bool,
) -> (f64, f64) {
    let dir = if clockwise { -1.0 } else { 1.0 };
    let end = (start_deg as f64 + dir * sweep_deg as f64).to_radians();
    (
        round4(cx + radius * end.cos()),
        round4(cy + radius * end.sin()),
    )
}

/// Runs a centre-form arc from a circle's start point and returns its extent.
/// The start, end and centre offsets are the printed values, so the
/// interpreter sees exactly what a program would say.
fn arc(
    cx: f64,
    cy: f64,
    radius: f64,
    start_deg: u32,
    sweep_deg: u32,
    clockwise: bool,
) -> gcode_sender::gcode::Extent {
    let start = start_point(cx, cy, radius, start_deg);
    let (ex, ey) = endpoint(cx, cy, radius, start_deg, sweep_deg, clockwise);
    let ix = round4(cx - start[0]);
    let iy = round4(cy - start[1]);
    let g = if clockwise { "G2" } else { "G3" };
    let mut m = Interpreter::new(start, [[0.0; 3]; 6], 54);
    m.step(&format!("{g} X{ex:.4} Y{ey:.4} I{ix:.4} J{iy:.4}"))
        .unwrap()
        .unwrap()
}
