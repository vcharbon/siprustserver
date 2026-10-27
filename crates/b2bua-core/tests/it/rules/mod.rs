//! The rule tests, one module per concern: outcome tests drive turns through
//! the turn driver; selection and matcher tests read a call through the one
//! context builder, action-primitive tests run on the one executor builder,
//! and the funnel and property tests hold over hand-built records
//! ([`fixtures`]).

use b2bua_core::config::B2buaConfig;
use b2bua_core::effects::{BufferedObservabilityEffect, CriticalStateEffect, HandlerResult};
use b2bua_core::initial_invite::build_initial_call;
use b2bua_core::obligations::ObligationSet;
use b2bua_core::rule_fixture::{context, executor, Varied};
use b2bua_core::rules::{
    default_rules, execute_rules, invariants, pick_ranked, ActionExecutor, Effect, Match,
    RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, TimerDelay, SERVICE_LAYER,
};
use b2bua_core::turn_notes::TurnNotes;
use b2bua_core::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::{Body, BodyAuthor, LegEnd};
use call::{
    CallModelState, Dialog, DialogState, EndingBy, Leg, LegKind, LegState, MachineId,
    OutboundInvite, PeerDialog, RemoteInfo, StateLabel, TerminationCause, TimerKey, TimerType,
};
use sip_ids::TurnIds;
use sip_message::generators::{
    generate_out_of_dialog_request, CapabilitySet, GenerateOutOfDialogRequestOpts,
    OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{HeaderName, Method, SipMessage, SipParser, SipRequest, SipStr};
use std::net::SocketAddr;

mod ack_leg_body;
mod answer_a_leg_new_dialog;
mod answered_2xx_discharge;
mod body_descriptors;
mod bye_order;
mod charging_in_dialog;
mod create_leg_admission;
mod decision_folds;
mod declared_capabilities;
mod default_sdp_create_leg;
mod enforce_equivalence;
mod final_guard;
mod fixtures;
mod going_away_gate;
mod header_update_removal_withholds_a_relayed_name;
mod keepalive;
mod ladder_give_up;
mod lifecycle_delivery;
mod limiter_refresh_armed;
mod limiter_settle;
mod machines;
mod media_primitives;
mod method_match;
mod provisionals;
mod reaper_verdicts;
mod route_set;
mod sdp_body_author;
mod service_timers;
mod stated_text_skips;
mod subscription_record;
mod teardown;
mod timer_fires;
mod turn_invariants;
mod txn_timeouts;
mod update_offer;
