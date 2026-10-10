use super::*;

/// The load driver mints the declarative actor body from a shape that
/// declares one, and nothing from a functional-only shape.
#[test]
#[ignore = "slow lane: loadgen"]
fn from_shape_mints_the_actor_body() {
    let inputs = ScenarioInputs::default();

    let actor = ShapeDescriptor::new("act")
        .load_shared(Arc::new(scenario_harness::actor::scenarios::BasicCall));
    let ae = MixEntry::from_shape(&actor, &inputs, 1.0).expect("actor shape has a body");
    assert!(matches!(ae.body, LoadBody::Actor(_)), "actor shape → actor body");

    let bare = ShapeDescriptor::new("bare");
    assert!(
        MixEntry::from_shape(&bare, &inputs, 1.0).is_none(),
        "functional-only shape has no body"
    );
}
