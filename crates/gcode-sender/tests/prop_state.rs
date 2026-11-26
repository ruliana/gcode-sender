//! Property tests for the state that outlives a job or a restart: work
//! offsets, boundaries, settings, the recorded tool position, and what the
//! job store writes and reads back.
//!
//! Some of these run the HTTP-facing parsers over arbitrary JSON, since that
//! is what a client can send.

use gcode_sender::bounds::Bounds;
use gcode_sender::position::{self, Check, Position, Record, TOLERANCE};
use gcode_sender::settings::{JogSettings, MAX_JOG_FEED, PauseOptions, PauseSettings, Settings};
use gcode_sender::store::JobStore;
use gcode_sender::work::{SYSTEMS, WorkOffsets, Zeros};
use proptest::prelude::*;

/// Millimetre values on a grid of thousandths, which print and read back
/// exactly.
fn mm(range: std::ops::RangeInclusive<i64>) -> impl Strategy<Value = f64> {
    range.prop_map(|v| v as f64 / 1000.0)
}

fn zeros() -> impl Strategy<Value = Zeros> {
    prop::collection::vec(mm(-500_000..=500_000), 18).prop_map(|v| {
        let mut zeros = [[0.0; 3]; 6];
        for (i, zero) in zeros.iter_mut().enumerate() {
            *zero = [v[3 * i], v[3 * i + 1], v[3 * i + 2]];
        }
        zeros
    })
}

fn point() -> impl Strategy<Value = [f64; 3]> {
    (
        mm(-500_000..=500_000),
        mm(-500_000..=500_000),
        mm(-500_000..=500_000),
    )
        .prop_map(|(x, y, z)| [x, y, z])
}

/// Any JSON document a client might send, nested a few levels deep.
fn json() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<f64>().prop_map(|f| serde_json::json!(f)),
        any::<i64>().prop_map(serde_json::Value::from),
        ".{0,12}".prop_map(serde_json::Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
            prop::collection::btree_map("[A-Za-z0-9_.]{0,8}", inner, 0..4)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}

proptest! {
    #[test]
    fn work_offsets_survive_a_round_trip_through_json(
        system in prop::sample::select(SYSTEMS.to_vec()),
        zeros in zeros(),
    ) {
        let work = WorkOffsets { system, zeros };
        let back: WorkOffsets = serde_json::from_value(serde_json::to_value(&work).unwrap()).unwrap();
        prop_assert_eq!(back, work);
    }

    #[test]
    fn work_offsets_refuse_systems_that_do_not_exist(
        number in prop_oneof![0u32..200, Just(53u32), Just(60u32)],
        decimal in prop::sample::select(vec!["", ".1", ".2", ".3"]),
    ) {
        prop_assume!(!(54..=59).contains(&number) || !decimal.is_empty());
        let doc = serde_json::json!({
            "system": format!("G{number}{decimal}"),
            "offsets": {}
        });
        prop_assert!(serde_json::from_value::<WorkOffsets>(doc).is_err());
    }

    #[test]
    fn stored_documents_never_panic_the_readers(text in ".{0,200}") {
        let _ = serde_json::from_str::<WorkOffsets>(&text);
        let _ = serde_json::from_str::<Bounds>(&text);
        let _ = serde_json::from_str::<Settings>(&text);
        let _ = serde_json::from_str::<PauseOptions>(&text);
        let _ = serde_json::from_str::<Position>(&text);
    }

    #[test]
    fn arbitrary_json_never_panics_the_readers(doc in json()) {
        let text = doc.to_string();
        let _ = serde_json::from_str::<WorkOffsets>(&text);
        let _ = serde_json::from_str::<Bounds>(&text);
        let _ = serde_json::from_str::<Settings>(&text);
        let _ = serde_json::from_str::<PauseOptions>(&text);
    }

    #[test]
    fn a_boundary_is_accepted_only_when_its_range_is_ordered_and_finite(
        low in prop::num::f64::ANY,
        high in prop::num::f64::ANY,
    ) {
        let bounds = Bounds { x: Some([low, high]), y: None, z: None };
        let valid = low.is_finite() && high.is_finite() && low <= high;
        prop_assert_eq!(bounds.validate().is_ok(), valid);
    }

    #[test]
    fn a_point_is_inside_its_boundary_exactly_when_it_is_between_the_limits(
        low in mm(-100_000..=100_000),
        span in mm(0..=100_000),
        at in mm(-150_000..=150_000),
    ) {
        let high = low + span;
        let bounds = Bounds { x: Some([low, high]), y: None, z: None };
        let inside = at >= low - 1e-6 && at <= high + 1e-6;
        prop_assert_eq!(bounds.check_point([Some(at), None, None]).is_ok(), inside);
        // An axis that is not moved is never checked.
        prop_assert!(bounds.check_point([None, Some(at), None]).is_ok());
    }

    #[test]
    fn a_jog_feed_is_accepted_only_when_it_is_positive_finite_and_not_too_fast(feed in prop::num::f64::ANY) {
        let jog = JogSettings { xy_feed: feed, z_feed: 300.0 };
        let valid = feed.is_finite() && feed > 0.0 && feed <= MAX_JOG_FEED;
        prop_assert_eq!(jog.validate().is_ok(), valid);
    }

    #[test]
    fn a_move_using_both_speeds_runs_at_the_slower_one(
        xy in mm(1..=10_000),
        z in mm(1..=10_000),
    ) {
        let jog = JogSettings { xy_feed: xy, z_feed: z };
        prop_assert_eq!(jog.feed_for([true, false, true]), xy.min(z));
        prop_assert_eq!(jog.feed_for([false, true, true]), xy.min(z));
        prop_assert_eq!(jog.feed_for([true, true, false]), xy);
        prop_assert_eq!(jog.feed_for([false, false, true]), z);
    }

    #[test]
    fn a_pause_override_is_accepted_only_when_the_result_is_valid(
        height in prop::num::f64::ANY,
        z in prop::option::of(prop::num::f64::ANY),
    ) {
        let options = PauseOptions { lift: None, height: Some(height), z };
        let valid = height.is_finite()
            && height > 0.0
            && height <= 500.0
            && z.is_none_or(|z| z.is_finite());
        prop_assert_eq!(options.apply(&PauseSettings::default()).is_ok(), valid);
    }

    #[test]
    fn a_recorded_difference_is_detected_only_past_the_tolerance(
        a in point(),
        d in mm(-100..=100),
    ) {
        prop_assume!((d.abs() - TOLERANCE).abs() > 1e-9);
        let b = position::add(a, [d, 0.0, 0.0]);
        prop_assert_eq!(position::differs(a, b), d.abs() > TOLERANCE);
        prop_assert_eq!(position::differs(b, a), position::differs(a, b));
        let moved = position::delta(a, b);
        prop_assert!((moved[0] - d).abs() < 1e-9);
    }

    #[test]
    fn shifting_every_zero_moves_each_of_them_by_the_same_amount(
        z in zeros(),
        d in mm(-100..=100),
    ) {
        prop_assume!((d.abs() - TOLERANCE).abs() > 1e-9);
        let shift = [d, -d, 0.0];
        let moved = position::shift_zeros(z, shift);
        prop_assert_eq!(position::zeros_differ(z, moved), d.abs() > TOLERANCE);
        for (before, after) in z.iter().zip(moved) {
            prop_assert!((after[0] - before[0] - d).abs() < 1e-9);
            prop_assert!((after[1] - before[1] + d).abs() < 1e-9);
        }
    }

    #[test]
    fn a_position_record_is_read_back_as_it_was_written(
        machine in point(),
        zeros in zeros(),
        recorded in point(),
        question in "[ -~]{0,40}",
        adjusted in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path()).unwrap();
        let position = Position {
            record: Some(Record { machine, zeros }),
            check: Some(Check {
                recorded,
                reported: machine,
                previous_zeros: zeros,
                zeros,
                adjusted,
                question,
            }),
        };
        store.set_position(&position).unwrap();
        prop_assert_eq!(store.position().unwrap(), position);
    }

    #[test]
    fn settings_and_boundaries_are_read_back_as_they_were_written(
        xy in mm(1..=10_000),
        z in mm(1..=10_000),
        height in mm(1..=500_000),
        dwell in mm(0..=60_000),
        low in mm(-100_000..=0),
        high in mm(0..=100_000),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path()).unwrap();
        let settings = Settings {
            jog: JogSettings { xy_feed: xy, z_feed: z },
            pause: PauseSettings {
                height,
                dwell_s: dwell,
                ..PauseSettings::default()
            },
            ..Settings::default()
        };
        prop_assert!(settings.validate().is_ok());
        store.set_settings(&settings).unwrap();
        prop_assert_eq!(store.settings().unwrap(), settings);

        let bounds = Bounds { x: Some([low, high]), y: None, z: Some([low, high]) };
        store.set_bounds(Some(&bounds)).unwrap();
        prop_assert_eq!(store.bounds().unwrap(), Some(bounds));
        store.set_bounds(None).unwrap();
        prop_assert_eq!(store.bounds().unwrap(), None);
    }
}
