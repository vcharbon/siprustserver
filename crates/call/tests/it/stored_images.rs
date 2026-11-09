//! Every field the record keeps a message in as bytes is an image the record
//! holds: one each, none left out.

use call::helpers::stored_images;
use call::model::*;

use crate::model_helpers::{settled_call, B_LOCAL_TAG, B_TAG};

fn sent(image: &str) -> SentRequest {
    SentRequest {
        branch: format!("z9hG4bK-{image}"),
        image: image.as_bytes().to_vec(),
        destination: HostPort { host: "192.0.2.77".into(), port: 5070 },
    }
}

fn emission(image: &str) -> RetainedEmission {
    RetainedEmission::on_trigger(
        image.as_bytes().to_vec(),
        ("192.0.2.77".into(), 5070),
        Repeated::response("INVITE", 200),
    )
}

/// The images `call` holds, as text.
fn held(call: &Call) -> Vec<String> {
    stored_images(call).iter().map(|i| String::from_utf8_lossy(i).to_string()).collect()
}

/// A record of the INVITE numbered `cseq` that sent `image` and whose exchange
/// holds each of the given emissions.
fn invite_record(cseq: i64, image: &str, exchange: InviteExchange) -> RequestRecord {
    RequestRecord { invite: Some(exchange), ..call::testkit::own_invite(cseq, Some(sent(image))) }
}

#[test]
fn the_record_lists_every_image_it_holds() {
    let mut call = settled_call();
    let mut expected: Vec<&str> = Vec::new();

    let exchange = InviteExchange {
        rejection: Some(emission("a-rejection")),
        provisional: Some(emission("a-provisional")),
        ..InviteExchange::default()
    };
    call.a_leg.invite = Some(invite_record(1, "a-invite", exchange));
    expected.extend(["a-invite", "a-rejection", "a-provisional"]);

    let leg = call.b_legs.first_mut().expect("the callee's leg");
    let dialog = leg.dialogs.by_tags_mut(B_LOCAL_TAG, B_TAG).expect("its dialog");
    let exchange = InviteExchange {
        emitted_ack: Some(emission("sent-ack")),
        ack_ahead: Some(emission("sent-ack-ahead")),
        ..InviteExchange::default()
    };
    dialog.sent_invite = Some(invite_record(2, "sent-invite", exchange));
    expected.extend(["sent-invite", "sent-ack", "sent-ack-ahead"]);

    let exchange = InviteExchange {
        answered_2xx: Some(Unacked2xx {
            dialog_tag: "t".into(),
            cseq: 3,
            emission: emission("answered-2xx"),
        }),
        ..InviteExchange::default()
    };
    dialog.answered_invite =
        Some(RequestRecord { invite: Some(exchange), ..RequestRecord::received_invite(3) });
    expected.push("answered-2xx");

    dialog.requests.push(RequestRecord::own(
        "INFO",
        4,
        sent("own-info"),
        30_000,
        DialogUsage::Session,
    ));
    expected.push("own-info");

    call.answered.push(AnsweredRequest {
        method: "INFO".into(),
        branch: "z9hG4bK-info".into(),
        response: emission("answered-info"),
        until_ms: 30_000,
    });
    expected.push("answered-info");

    call.provisionals.push(call::testkit::shown_provisional(
        "b-1",
        B_TAG,
        1,
        1,
        "shown",
        1,
        Ladder::Repeating(emission("shown-ladder")),
        ResponderAck::Pending,
    ));
    expected.push("shown-ladder");

    let got = held(&call);
    for image in &expected {
        let times = got.iter().filter(|g| g == image).count();
        assert_eq!(times, 1, "{image}: {got:?}");
    }
}
