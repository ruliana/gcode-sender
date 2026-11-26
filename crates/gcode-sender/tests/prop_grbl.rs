//! Property tests for what the controller prints: replies, status reports
//! and the bracketed lines that answer `$I`, `$#`, `$G` and `$` queries.
//!
//! The controller's text is the one input this program does not choose, and
//! line noise can arrive around any of it, so these tests feed the parser
//! arbitrary text and damaged replies as well as well-formed ones.

use gcode_sender::grbl::{self, Response};
use proptest::prelude::*;

/// Characters that show up in line noise: controls, a replacement character,
/// spaces, and the delimiters the protocol uses.
fn noise_char() -> impl Strategy<Value = char> {
    prop::sample::select(vec![
        '\0', '\u{7}', '\r', '\n', '\t', ' ', '\u{FFFD}', '<', '>', '[', ']', '|', ':', ',',
    ])
}

fn noise() -> impl Strategy<Value = String> {
    prop::collection::vec(noise_char(), 0..4).prop_map(|cs| cs.into_iter().collect())
}

/// Only what the parser trims from the ends of a line: whitespace, control
/// bytes and the replacement character that line noise decodes to.
fn trimmed_noise() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec!['\0', '\u{7}', '\r', '\n', '\t', ' ', '\u{FFFD}']),
        0..4,
    )
    .prop_map(|cs| cs.into_iter().collect())
}

/// A coordinate with three decimals, as the controller prints them. Built
/// from thousandths so that printing and parsing agree exactly.
fn coord() -> impl Strategy<Value = f64> {
    (-100_000i64..=100_000).prop_map(|thousandths| thousandths as f64 / 1000.0)
}

fn fields(values: &[f64]) -> Vec<String> {
    values.iter().map(|v| format!("{v:.3}")).collect()
}

const STATES: [&str; 8] = [
    "Idle", "Run", "Hold", "Jog", "Alarm", "Door", "Home", "Check",
];

proptest! {
    #[test]
    fn any_text_can_be_parsed_without_panicking(line in prop::collection::vec(any::<char>(), 0..300)) {
        let line: String = line.into_iter().collect();
        let _ = grbl::parse(&line);
    }

    #[test]
    fn replies_keep_their_meaning_with_noise_around_them(
        code in 0u32..1000,
        before in trimmed_noise(),
        after in trimmed_noise(),
    ) {
        prop_assert_eq!(grbl::parse(&format!("{before}ok{after}")), Response::Ok);
        prop_assert_eq!(
            grbl::parse(&format!("{before}error:{code}{after}")),
            Response::Error(code)
        );
        prop_assert_eq!(
            grbl::parse(&format!("{before}ALARM:{code}{after}")),
            Response::Alarm(code)
        );
    }

    #[test]
    fn a_well_formed_status_report_gives_back_what_was_printed(
        state in prop::sample::select(STATES.to_vec()),
        mpos in prop::collection::vec(coord(), 3),
        wpos in prop::collection::vec(coord(), 3),
        feed in 0u32..100_000,
    ) {
        let line = format!(
            "<{state}|MPos:{}|WPos:{}|FS:{feed},0>",
            fields(&mpos).join(","),
            fields(&wpos).join(",")
        );
        let Response::Status(s) = grbl::parse(&line) else {
            return Err(TestCaseError::fail(format!("not read as a status: {line}")));
        };
        prop_assert_eq!(s.state.as_str(), state);
        prop_assert_eq!(s.machine_position, Some(mpos));
        prop_assert_eq!(s.work_position, Some(wpos));
        prop_assert_eq!(s.feed_rate, Some(feed as f64));
        prop_assert_eq!(s.spindle_speed, Some(0.0));
    }

    /// A field that is not a number must not shift the fields after it onto
    /// the wrong axes. The position is either read whole or not at all.
    #[test]
    fn a_damaged_position_field_is_never_read_with_shifted_axes(
        mpos in prop::collection::vec(coord(), 3),
        junk in "[a-zA-Z.+-]{1,4}",
        at in 0usize..3,
        extra in prop::collection::vec(coord(), 0..2),
    ) {
        let mut parts = fields(&mpos);
        parts[at] = junk.clone();
        parts.extend(fields(&extra));
        let line = format!("<Run|MPos:{}|FS:0,0>", parts.join(","));
        let Response::Status(s) = grbl::parse(&line) else {
            return Err(TestCaseError::fail(format!("not read as a status: {line}")));
        };
        prop_assert!(
            s.machine_position.is_none(),
            "a damaged position was read as {:?} from {line}",
            s.machine_position
        );
    }

    #[test]
    fn a_damaged_work_zero_line_is_not_read(
        system in prop::sample::select(vec![54u32, 55, 56, 57, 58, 59]),
        zero in prop::collection::vec(coord(), 3),
        at in 0usize..3,
        junk in "[a-zA-Z]{1,3}",
    ) {
        let mut parts = fields(&zero);
        parts[at] = junk;
        let body = parts.join(",");
        let parsed = grbl::parse(&format!("[G{system}:{body}]"));
        let Response::Message { tag, body } = parsed else {
            return Err(TestCaseError::fail("not read as a message"));
        };
        prop_assert_eq!(grbl::work_zero_message(&tag, &body), None);
    }

    #[test]
    fn a_work_zero_line_gives_its_system_index_and_zero(
        system in prop::sample::select(vec![54u32, 55, 56, 57, 58, 59]),
        zero in prop::collection::vec(coord(), 3),
    ) {
        let line = format!("[G{system}:{}]", fields(&zero).join(","));
        let Response::Message { tag, body } = grbl::parse(&line) else {
            return Err(TestCaseError::fail("not read as a message"));
        };
        let index = (system - 54) as usize;
        prop_assert_eq!(
            grbl::work_zero_message(&tag, &body),
            Some((index, [zero[0], zero[1], zero[2]]))
        );
    }

    #[test]
    fn the_active_system_is_the_first_g5x_word_on_a_gc_line(
        system in prop::sample::select(vec![54u32, 55, 56, 57, 58, 59]),
        other in prop::sample::select(vec![17u32, 21, 90, 91, 94, 93, 0, 1]),
    ) {
        let body = format!("G{other} G{system} G17 G21 G90 G94");
        prop_assert_eq!(grbl::active_system_message(&body), Some(system));
        prop_assert_eq!(grbl::active_system_message(&format!("G{other} G17")), None);
    }

    #[test]
    fn opt_fields_are_read_by_position(
        version in "[A-Za-z0-9]{0,6}",
        planner in 0usize..100_000,
        rx in 0usize..100_000,
    ) {
        let body = format!("{version},{planner},{rx}");
        prop_assert_eq!(grbl::planner_blocks_from_opt(&body), Some(planner));
        prop_assert_eq!(grbl::rx_buffer_from_opt(&body), Some(rx));
    }

    #[test]
    fn messages_and_statuses_never_panic_on_their_numeric_helpers(body in noise()) {
        let _ = grbl::rx_buffer_from_opt(&body);
        let _ = grbl::planner_blocks_from_opt(&body);
        let _ = grbl::active_system_message(&body);
        let _ = grbl::work_zero_message("G54", &body);
        let _ = grbl::parse(&format!("<{body}>"));
        let _ = grbl::parse(&format!("[{body}]"));
    }
}
