//! The scripts the turn lane and its fuzz target start every case from: a
//! call dialled toward bob, answered, refused, rerouted, re-INVITEd,
//! updated, reliably provisioned, cancelled, referred and transferred; a
//! call under a session timer; and bob's early media under each masking
//! strategy.

use call::features::RelayFirst18xStrategy;
use call::TimerType;
use sip_message::Method;

use b2bua_core::turn_driver::Step;

use super::drive::Setup;

/// A script: its name, the setup its call is dialled under, its steps.
pub struct Script {
    pub name: &'static str,
    pub setup: Setup,
    pub steps: Vec<Step>,
}

/// bob's early media, as RFC 5009 marks it.
const EARLY_MEDIA: &[(&str, &str)] = &[("P-Early-Media", "sendrecv")];

/// bob's reliable early media (RFC 3262, RFC 5009).
const RELIABLE_EARLY_MEDIA: &[(&str, &str)] =
    &[("Require", "100rel"), ("RSeq", "1"), ("P-Early-Media", "sendrecv")];

/// bob's 2xx taking part in alice's session timer (RFC 4028 §9).
const TIMED_ANSWER: &[(&str, &str)] =
    &[("Require", "timer"), ("Session-Expires", "1800;refresher=uac"), ("Min-SE", "90")];

/// The scripts every case starts from.
pub fn scripts() -> Vec<Script> {
    let plain = |name, steps| Script { name, setup: Setup::Plain, steps };
    let mut all: Vec<Script> =
        plain_scripts().into_iter().map(|(name, steps)| plain(name, steps)).collect();
    all.push(Script {
        name: "session-timed",
        setup: Setup::SessionTimer,
        steps: vec![
            Step::AnswersWith("b-1", 200, TIMED_ANSWER.to_vec()),
            Step::AliceAcks,
            Step::Sends("a", Method::Invite),
            Step::AnswersSent("b-1", 200),
            Step::AliceAcks,
            Step::Sends("a", Method::Bye),
            Step::AnswersSent("b-1", 200),
        ],
    });
    for (name, strategy) in [
        ("early-media-drop-sdp", RelayFirst18xStrategy::DropSdp),
        ("early-media-keep-sdp", RelayFirst18xStrategy::KeepSdp),
    ] {
        all.push(Script {
            name,
            setup: Setup::EarlyMedia(strategy),
            steps: vec![
                Step::AnswersWith("b-1", 183, EARLY_MEDIA.to_vec()),
                Step::Answers("b-1", 180),
                Step::Answers("b-1", 200),
                Step::AliceAcks,
                Step::Sends("a", Method::Bye),
                Step::AnswersSent("b-1", 200),
            ],
        });
    }
    all.push(Script {
        name: "early-media-fake-prack",
        setup: Setup::EarlyMedia(RelayFirst18xStrategy::FakePrack),
        steps: vec![
            Step::AnswersWith("b-1", 183, RELIABLE_EARLY_MEDIA.to_vec()),
            Step::AnswersLast("b-1", Method::Prack, 200),
            Step::Answers("b-1", 200),
            Step::AliceAcks,
            Step::Sends("a", Method::Bye),
            Step::AnswersSent("b-1", 200),
        ],
    });
    all.push(Script {
        name: "early-media-promoted",
        setup: Setup::EarlyMedia(RelayFirst18xStrategy::PromotePemTo200),
        steps: vec![
            Step::AnswersWith("b-1", 183, EARLY_MEDIA.to_vec()),
            Step::AliceAcks,
            Step::Answers("b-1", 200),
            Step::Sends("a", Method::Bye),
            Step::AnswersSent("b-1", 200),
        ],
    });
    all.push(plain(
        "referred-then-transferred",
        vec![
            Step::Answers("b-1", 200),
            Step::AliceAcks,
            Step::Sends("b-1", Method::Refer),
            Step::AnswersSent("b-1", 200),
            Step::ReferAnswers {
                outcome: "allow",
                payload: serde_json::json!({"destination": {"host": "127.0.0.1", "port": 5071}}),
            },
            Step::Answers("b-2", 180),
            Step::Answers("b-2", 200),
            Step::AnswersSent("b-2", 200),
            Step::AnswersSent("a", 200),
            Step::AnswersSent("b-1", 200),
            Step::Sends("a", Method::Bye),
            Step::AnswersSent("b-2", 200),
        ],
    ));
    all
}

/// The scripts of a plain call, by name.
fn plain_scripts() -> Vec<(&'static str, Vec<Step>)> {
    let route = serde_json::json!({
        "destination": {"host": "127.0.0.1", "port": 5071},
        "new_ruri": "sip:carol@127.0.0.1:5071",
        "features": serde_json::to_value(b2bua_core::decision::default_platform_features())
            .unwrap_or_default(),
    });
    vec![
        (
            "answered-bob-hangs-up",
            vec![
                Step::Answers("b-1", 180),
                Step::Answers("b-1", 200),
                Step::AliceAcks,
                Step::Sends("b-1", Method::Bye),
            ],
        ),
        (
            "answered-alice-hangs-up",
            vec![
                Step::Answers("b-1", 183),
                Step::Answers("b-1", 200),
                Step::Sends("a", Method::Bye),
            ],
        ),
        (
            "refused-then-rejected",
            vec![
                Step::Answers("b-1", 180),
                Step::Answers("b-1", 486),
                Step::FailureAnswers {
                    outcome: "reject",
                    payload: serde_json::json!({"code": 480, "reason": "Temporarily Unavailable"}),
                },
            ],
        ),
        (
            "refused-then-rerouted",
            vec![
                Step::Answers("b-1", 486),
                Step::FailureAnswers { outcome: "failover", payload: route },
            ],
        ),
        (
            "refused-then-redirected",
            vec![
                Step::Answers("b-1", 486),
                Step::FailureAnswers {
                    outcome: "redirect",
                    payload: serde_json::json!({
                        "code": 302,
                        "contacts": [{"uri": "sip:carol@127.0.0.1:5071", "q": 0.5}],
                    }),
                },
            ],
        ),
        ("ringing-unanswered", vec![Step::Answers("b-1", 180), Step::Fires(TimerType::NoAnswer)]),
        ("setup-unanswered", vec![Step::Fires(TimerType::SetupTimeout)]),
        (
            "reinvited",
            vec![
                Step::Answers("b-1", 180),
                Step::Answers("b-1", 200),
                Step::AliceAcks,
                Step::Sends("a", Method::Invite),
                Step::AnswersSent("b-1", 200),
                Step::AliceAcks,
                Step::Sends("a", Method::Bye),
                Step::AnswersSent("b-1", 200),
            ],
        ),
        (
            "updated",
            vec![
                Step::Answers("b-1", 200),
                Step::AliceAcks,
                Step::Sends("a", Method::Update),
                Step::AnswersSent("b-1", 200),
                Step::Sends("b-1", Method::Bye),
                Step::AnswersSent("a", 200),
            ],
        ),
        (
            "reliably-provisioned",
            vec![
                Step::AnswersReliably("b-1", 183),
                Step::AnswersSent("b-1", 200),
                Step::Answers("b-1", 200),
                Step::AliceAcks,
                Step::Sends("a", Method::Bye),
                Step::AnswersSent("b-1", 200),
            ],
        ),
        (
            "cancelled",
            vec![
                Step::Answers("b-1", 180),
                Step::AliceCancels,
                Step::AnswersSent("b-1", 200),
                Step::Answers("b-1", 487),
            ],
        ),
        (
            "referred-then-refused",
            vec![
                Step::Answers("b-1", 200),
                Step::AliceAcks,
                Step::Sends("b-1", Method::Refer),
                Step::AnswersSent("b-1", 200),
                Step::ReferAnswers {
                    outcome: "reject",
                    payload: serde_json::json!({"reject_code": 603, "reject_reason": "Declined"}),
                },
                Step::AnswersSent("b-1", 200),
                Step::Sends("b-1", Method::Bye),
                Step::AnswersSent("a", 200),
            ],
        ),
    ]
}
