//! Property test for the HTTP input handling, against the real server and a
//! simulated controller.
//!
//! Whatever a client sends, the sender must answer with a success or a client
//! error. A `500` means something the sender did not expect went wrong, and
//! the server must still be serving afterwards.

mod common;

use common::Harness;
use grbl_sim::SimConfig;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use reqwest::StatusCode;
use serde_json::{Value, json};

/// Routes that take a JSON body, with the method each one uses.
const JSON_ROUTES: [(&str, &str); 8] = [
    ("PUT", "/settings"),
    ("PUT", "/machine/bounds"),
    ("POST", "/machine/jog"),
    ("PUT", "/machine/work-offset"),
    ("PUT", "/machine/work-system"),
    ("POST", "/machine/position-check"),
    ("POST", "/jobs/no-such-job/pause"),
    ("POST", "/pairing"),
];

/// Any JSON document a client might send, nested a few levels deep. Numbers
/// include the extremes, and keys are drawn from the names the API uses
/// along with some that it does not.
fn json_document() -> impl Strategy<Value = Value> {
    let key = prop_oneof![
        Just("x".to_string()),
        Just("y".to_string()),
        Just("z".to_string()),
        Just("feed".to_string()),
        Just("mode".to_string()),
        Just("space".to_string()),
        Just("system".to_string()),
        Just("offsets".to_string()),
        Just("bounds".to_string()),
        Just("pause".to_string()),
        Just("height".to_string()),
        Just("lift".to_string()),
        Just("code".to_string()),
        Just("moved".to_string()),
        "[ -~]{0,10}",
    ];
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<f64>().prop_map(|f| json!(f)),
        prop::num::f64::ANY.prop_map(|f| json!(f)),
        any::<i64>().prop_map(Value::from),
        "[ -~]{0,12}".prop_map(Value::from),
        prop::sample::select(vec![
            "G54", "G55", "G59", "G60", "Machine", "work", "absolute"
        ])
        .prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec((key.clone(), inner), 0..5)
                .prop_map(|pairs| Value::Object(pairs.into_iter().collect())),
        ]
    })
}

#[test]
fn no_request_can_make_the_sender_fail() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = rt.block_on(Harness::start(SimConfig::grbl()));
    let mut runner = TestRunner::new(Config {
        cases: 256,
        failure_persistence: None,
        ..Config::default()
    });
    let strategy = (
        prop::sample::select(JSON_ROUTES.to_vec()),
        json_document(),
        "[ -~\\n]{0,200}",
    );
    runner
        .run(&strategy, |((method, path), body, program)| {
            let (status, reply) = rt.block_on(async {
                match method {
                    "PUT" => h.put_json(path, &body).await,
                    _ => h.post_json(path, &body).await,
                }
            });
            prop_assert!(
                status != StatusCode::INTERNAL_SERVER_ERROR,
                "{method} {path} with {body} gave {status}: {reply}"
            );

            let (status, reply) = rt.block_on(h.post_body("/jobs?name=prop", &program));
            prop_assert!(
                status != StatusCode::INTERNAL_SERVER_ERROR,
                "uploading {program:?} gave {status}: {reply}"
            );
            Ok(())
        })
        .unwrap();

    let (status, _) = rt.block_on(h.get("/health"));
    assert_eq!(status, StatusCode::OK, "the sender stopped serving");
}
