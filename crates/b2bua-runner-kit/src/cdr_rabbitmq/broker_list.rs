//! The broker list of the RabbitMQ CDR sink's grammar,
//! `B2BUA_CDR_RABBITMQ_URL`: one AMQP URI or several separated by commas, one
//! per node of a cluster, and how logs name each broker.

use std::net::Ipv6Addr;

use lapin::uri::AMQPUri;

/// The variable listing the brokers.
pub(super) const URL_VARIABLE: &str = "B2BUA_CDR_RABBITMQ_URL";

/// A broker as logs name it: `host:port`, an IPv6 host bracketed
/// (`[2001:db8::1]:5672`), never its credentials.
pub(super) fn broker_name(uri: &AMQPUri) -> String {
    let host = uri.authority.host.as_str();
    match host.parse::<Ipv6Addr>() {
        Ok(_) => format!("[{host}]:{}", uri.authority.port),
        Err(_) => format!("{host}:{}", uri.authority.port),
    }
}

/// Whether `a` and `b` name one node: the same port on the same host, its
/// name compared without case.
fn same_node(a: &AMQPUri, b: &AMQPUri) -> bool {
    a.authority.port == b.authority.port && a.authority.host.eq_ignore_ascii_case(&b.authority.host)
}

/// The brokers `raw` lists, comma-separated, each checked by
/// [`amqp_connect::checked_uri`]. `Err` names the entry, by position, that is
/// empty, malformed, a node listed before (same host and port), or of a
/// scheme other than the first entry's: a list mixing `amqp` and `amqps`
/// would send records in clear whenever a plain node answers.
pub(super) fn broker_list(raw: &str) -> Result<Vec<AMQPUri>, String> {
    let mut brokers: Vec<AMQPUri> = Vec::new();
    for (index, entry) in raw.split(',').map(str::trim).enumerate() {
        let entry_name = format!("{URL_VARIABLE} entry {}", index + 1);
        if entry.is_empty() {
            return Err(format!(
                "{entry_name} is empty: list AMQP URIs separated by single commas (a comma \
                 inside a URI is written %2C)"
            ));
        }
        let uri = amqp_connect::checked_uri(&entry_name, entry)?;
        if let Some(first) = brokers.first().filter(|first| first.scheme != uri.scheme) {
            return Err(format!(
                "{entry_name} is {:?} while entry 1 is {:?}: every broker of the list runs \
                 the same scheme",
                uri.scheme, first.scheme
            ));
        }
        if let Some(earlier) = brokers.iter().find(|b| same_node(b, &uri)) {
            return Err(format!("{entry_name} lists {} again", broker_name(earlier)));
        }
        brokers.push(uri);
    }
    Ok(brokers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comma_separated_url_lists_every_broker_in_order() {
        let list = "amqp://u:p@node-a:5672/%2f, amqp://u:p@node-b/%2f,amqp://u:p@node-a:5673/%2f";
        let brokers = broker_list(list).expect("a list of three");
        let names: Vec<String> = brokers.iter().map(broker_name).collect();
        assert_eq!(names, ["node-a:5672", "node-b:5672", "node-a:5673"]);
        assert_eq!(brokers[1].authority.userinfo.password, "p");
    }

    #[test]
    fn an_ipv6_broker_is_named_bracketed_and_one_address_is_one_node() {
        let brokers = broker_list("amqp://u:p@[2001:db8::1]/%2f,amqp://u:p@[2001:db8::2]:5673/%2f")
            .expect("a list of two");
        let names: Vec<String> = brokers.iter().map(broker_name).collect();
        assert_eq!(names, ["[2001:db8::1]:5672", "[2001:db8::2]:5673"]);
        let twice = "amqp://u:p@[2001:db8::1]/%2f,amqp://u:p@[2001:DB8:0::1]:5672/%2f";
        let e = broker_list(twice).expect_err("one address spelt twice is one node");
        assert!(e.contains("entry 2 lists [2001:db8::1]:5672 again"), "{e}");
    }

    /// Each refusal names the variable and the entry's position, never the
    /// entry itself (it may carry a password).
    #[test]
    fn a_broker_list_with_an_unusable_entry_is_refused_naming_the_entry() {
        for (list, entry, why) in [
            ("amqp://a/%2f,,amqp://b/%2f", "entry 2", "empty"),
            ("amqp://a/%2f,", "entry 2", "empty"),
            ("amqp://a/%2f,http://u:secret@b/", "entry 2", "not an AMQP URI"),
            ("amqp://a/%2f,amqp://b/%2f?heartbeat=0", "entry 2", "heartbeat=0"),
            ("amqp://a:5672/%2f,amqp://A/other", "entry 2", "a:5672 again"),
            ("amqp://a/%2f,amqps://u:secret@b/%2f", "entry 2", "same scheme"),
        ] {
            let e = broker_list(list).expect_err("an unusable entry refuses boot");
            assert!(e.contains(&format!("B2BUA_CDR_RABBITMQ_URL {entry}")), "{list}: {e}");
            assert!(e.contains(why), "{list}: {e}");
            assert!(!e.contains("secret"), "{list}: {e}");
        }
    }
}
