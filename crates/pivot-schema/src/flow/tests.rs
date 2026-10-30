use super::*;

#[test]
fn an_anchor_round_trips_through_its_token() {
    for token in ["trigger", "step:s1", "step:invite-out"] {
        assert_eq!(token.parse::<Anchor>().unwrap().to_string(), token);
    }
}

#[test]
fn a_malformed_anchor_is_refused() {
    for token in ["step:", "start", "", "s1"] {
        assert!(token.parse::<Anchor>().is_err(), "{token:?} should be refused");
    }
}

fn node(text: &str) -> FlowNode {
    serde_json::from_str(text).unwrap()
}

const DELAY: &str = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;

#[test]
fn each_op_decodes_to_its_own_node_kind() {
    let step = format!(
        r#"{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{DELAY}}}"#
    );
    assert!(matches!(node(&step), FlowNode::Message(_)));
    assert!(matches!(
        node(r#"{"id":"i1","op":"inject","action":"node-kill","target":"b"}"#),
        FlowNode::Inject(_)
    ));
    assert!(matches!(
        node(r#"{"id":"a1","op":"alt","branches":[{"name":"x","steps":[]}]}"#),
        FlowNode::Alt(_)
    ));
    assert!(matches!(node(r#"{"id":"u1","op":"unordered","steps":[]}"#), FlowNode::Unordered(_)));
}

/// The reason [`FlowNode`] dispatches by hand: an untagged enum would
/// report "matched no variant" and hide the field that is actually wrong.
#[test]
fn a_misspelled_field_names_itself_in_the_error() {
    let text = format!(
        r#"{{"id":"s1","leg":"A","op":"send","mgs":{{"method":"INVITE"}},"delay":{DELAY}}}"#
    );
    let error = serde_json::from_str::<FlowNode>(&text).unwrap_err().to_string();
    assert!(error.contains("mgs"), "{error}");
}

#[test]
fn an_unknown_op_is_refused_by_name() {
    let error =
        serde_json::from_str::<FlowNode>(r#"{"id":"x","op":"snd"}"#).unwrap_err().to_string();
    assert!(error.contains("snd"), "{error}");
    assert!(serde_json::from_str::<FlowNode>(r#"{"id":"x"}"#).is_err());
}

#[test]
fn flags_are_omitted_when_false_so_emptiness_never_carries_meaning() {
    let text = format!(
        r#"{{"id":"s1","leg":"A","op":"send","msg":{{"method":"INVITE"}},"delay":{DELAY}}}"#
    );
    let FlowNode::Message(step) = node(&text) else { panic!("a message step") };
    assert!(!step.auto && !step.optional);
    let json = serde_json::to_string(&step).unwrap();
    assert!(!json.contains("auto") && !json.contains("optional"), "{json}");
}

#[test]
fn a_block_reports_the_steps_it_contains() {
    let alt = node(
        r#"{"id":"a1","op":"alt","branches":[
             {"name":"cancelled","steps":[{"id":"s2","leg":"A","op":"expect","msg":{"status":487},
               "delay":{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}}]},
             {"name":"answered","steps":[{"id":"s3","leg":"A","op":"expect","msg":{"status":200},
               "delay":{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}}]}]}"#,
    );
    assert_eq!(alt.steps().iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["s2", "s3"]);
    assert_eq!(alt.id(), "a1");
}
