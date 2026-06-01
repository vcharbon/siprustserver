pub mod g711;

pub use g711::{
    alaw_decode, alaw_encode, g711_round_trip, mulaw_decode, mulaw_encode, G711Codec,
};
