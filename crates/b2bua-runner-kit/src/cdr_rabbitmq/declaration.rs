//! The queue declaration of the RabbitMQ CDR sink, per [`CdrQueueDeclare`].

use lapin::{
    options::QueueDeclareOptions,
    types::{AMQPValue, FieldTable, LongString},
};

use super::settings::CdrQueueDeclare;

/// The queue declaration [`CdrQueueDeclare`] states.
pub(super) fn declaration(declare: CdrQueueDeclare) -> (QueueDeclareOptions, FieldTable) {
    let mut args = FieldTable::default();
    match declare {
        CdrQueueDeclare::Own { max_len } => {
            if max_len > 0 {
                // Drop the OLDEST record on overflow so a stalled consumer
                // never grows the broker without limit.
                args.insert("x-max-length".into(), AMQPValue::LongLongInt(max_len));
                args.insert(
                    "x-overflow".into(),
                    AMQPValue::LongString(LongString::from("drop-head")),
                );
            }
            (QueueDeclareOptions { durable: true, ..Default::default() }, args)
        }
        CdrQueueDeclare::Existing => {
            (QueueDeclareOptions { passive: true, ..Default::default() }, args)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owned_queue_is_declared_durable_and_bounded_drop_head() {
        let (opts, args) = declaration(CdrQueueDeclare::Own { max_len: 100_000 });
        assert!(opts.durable && !opts.passive);
        let inner = args.inner();
        assert_eq!(inner.get("x-max-length"), Some(&AMQPValue::LongLongInt(100_000)));
        assert_eq!(
            inner.get("x-overflow"),
            Some(&AMQPValue::LongString(LongString::from("drop-head")))
        );
    }

    #[test]
    fn an_owned_unbounded_queue_is_declared_durable_without_arguments() {
        let (opts, args) = declaration(CdrQueueDeclare::Own { max_len: 0 });
        assert!(opts.durable && !opts.passive);
        assert!(args.inner().is_empty());
    }

    #[test]
    fn a_broker_held_queue_is_declared_passively_without_arguments() {
        let (opts, args) = declaration(CdrQueueDeclare::Existing);
        assert!(opts.passive, "an existing queue is never (re)declared");
        assert!(args.inner().is_empty(), "no argument of the broker's queue is restated");
    }
}
