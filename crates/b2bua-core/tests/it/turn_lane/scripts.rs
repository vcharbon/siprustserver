//! The scripts the turn lane and its fuzz target start every case from: a
//! call dialled toward bob, answered, refused, rerouted, re-INVITEd,
//! updated, reliably provisioned, cancelled and referred.

use call::TimerType;
use sip_message::Method;

use super::drive::Step;

/// The scripts every case starts from, by name.
pub fn scripts() -> Vec<(&'static str, Vec<Step>)> {
    let route = serde_json::json!({
        "destination": {"host": "127.0.0.1", "port": 5071},
        "new_ruri": "sip:carol@127.0.0.1:5071",
        "features": serde_json::to_value(b2bua_core::decision::default_platform_features())
            .unwrap_or_default(),
    });
    vec![
        (
            "answered-bob-hangs-up",
            vec![Step::BobAnswers(180), Step::BobAnswers(200), Step::AliceAcks, Step::BobByes],
        ),
        (
            "answered-alice-hangs-up",
            vec![Step::BobAnswers(183), Step::BobAnswers(200), Step::AliceByes],
        ),
        (
            "refused-then-rejected",
            vec![
                Step::BobAnswers(180),
                Step::BobAnswers(486),
                Step::FailureAnswers {
                    outcome: "reject",
                    payload: serde_json::json!({"code": 480, "reason": "Temporarily Unavailable"}),
                },
            ],
        ),
        (
            "refused-then-rerouted",
            vec![
                Step::BobAnswers(486),
                Step::FailureAnswers { outcome: "failover", payload: route },
            ],
        ),
        (
            "refused-then-redirected",
            vec![
                Step::BobAnswers(486),
                Step::FailureAnswers {
                    outcome: "redirect",
                    payload: serde_json::json!({
                        "code": 302,
                        "contacts": [{"uri": "sip:carol@127.0.0.1:5071", "q": 0.5}],
                    }),
                },
            ],
        ),
        ("ringing-unanswered", vec![Step::BobAnswers(180), Step::Fires(TimerType::NoAnswer)]),
        ("setup-unanswered", vec![Step::Fires(TimerType::SetupTimeout)]),
        (
            "reinvited",
            vec![
                Step::BobAnswers(180),
                Step::BobAnswers(200),
                Step::AliceAcks,
                Step::AliceSends(Method::Invite),
                Step::BobAnswersSent(200),
                Step::AliceAcks,
                Step::AliceByes,
                Step::BobAnswersSent(200),
            ],
        ),
        (
            "updated",
            vec![
                Step::BobAnswers(200),
                Step::AliceAcks,
                Step::AliceSends(Method::Update),
                Step::BobAnswersSent(200),
                Step::BobByes,
                Step::AliceAnswersSent(200),
            ],
        ),
        (
            "reliably-provisioned",
            vec![
                Step::BobAnswersReliably(183),
                Step::BobAnswersSent(200),
                Step::BobAnswers(200),
                Step::AliceAcks,
                Step::AliceByes,
                Step::BobAnswersSent(200),
            ],
        ),
        (
            "cancelled",
            vec![
                Step::BobAnswers(180),
                Step::AliceCancels,
                Step::BobAnswersSent(200),
                Step::BobAnswers(487),
            ],
        ),
        (
            "referred-then-refused",
            vec![
                Step::BobAnswers(200),
                Step::AliceAcks,
                Step::BobRefers,
                Step::BobAnswersSent(200),
                Step::ReferAnswers {
                    outcome: "reject",
                    payload: serde_json::json!({"reject_code": 603, "reject_reason": "Declined"}),
                },
                Step::BobAnswersSent(200),
                Step::BobByes,
                Step::AliceAnswersSent(200),
            ],
        ),
    ]
}
