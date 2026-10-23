use super::rewrite_connection_and_ports;

/// The grammar contract: only `c=IN IP4` addresses and active `m=` ports
/// move; the `o=` line's address, attributes, pair counts, format lists and
/// the mixed line endings all ride byte-for-byte.
#[test]
fn only_the_named_fields_move_and_every_other_byte_rides() {
    let sdp = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\nc=IN IP4 1.2.3.4\r\n\
                   m=audio 5000/2 RTP/AVP 8 101 0\r\na=rtpmap:101 telephone-event/8000\n\
                   m=image 5008 udptl t38\r\n";
    let mut booked = vec![];
    let out = rewrite_connection_and_ports(sdp, Some("127.0.0.9"), |pairs| {
        booked.push(pairs);
        Some(41000 + 10 * booked.len() as u16)
    });
    assert_eq!(
        out,
        "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\nc=IN IP4 127.0.0.9\r\n\
             m=audio 41010/2 RTP/AVP 8 101 0\r\na=rtpmap:101 telephone-event/8000\n\
             m=image 41020 udptl t38\r\n"
    );
    assert_eq!(booked, [2, 1], "each active line offers its own pair count");
}

/// A port-0 stream is never offered for booking (RFC 3264 §5.1), a `None`
/// booking leaves its line as stored, and no address means no `c=` rewrite.
#[test]
fn rejected_streams_and_declined_bookings_ride_verbatim() {
    let sdp = "c=IN IP4 1.2.3.4\r\nm=audio 0 RTP/AVP 8\r\nm=image 6000 udptl t38\r\n";
    let mut offers = 0;
    let out = rewrite_connection_and_ports(sdp, None, |_| {
        offers += 1;
        None
    });
    assert_eq!(out, sdp);
    assert_eq!(offers, 1, "only the live stream was offered");
}

/// A line the m= grammar does not fit is not guessed at.
#[test]
fn an_unparsable_media_line_rides_verbatim() {
    let sdp = "m=audio\r\nm=audio five RTP/AVP 0\r\n";
    let out = rewrite_connection_and_ports(sdp, None, |_| Some(41000));
    assert_eq!(out, sdp);
}
