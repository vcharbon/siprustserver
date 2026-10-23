use super::*;
use std::io::Write;

/// Build a classic-pcap (LE, µs, LINKTYPE_RAW=101) file in memory.
fn pcap_raw_ip(records: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes()); // major
    f.extend_from_slice(&4u16.to_le_bytes()); // minor
    f.extend_from_slice(&[0u8; 8]); // thiszone + sigfigs
    f.extend_from_slice(&65535u32.to_le_bytes()); // snaplen
    f.extend_from_slice(&101u32.to_le_bytes()); // linktype RAW
    for (ts_us, pkt) in records {
        f.extend_from_slice(&((ts_us / 1_000_000) as u32).to_le_bytes());
        f.extend_from_slice(&((ts_us % 1_000_000) as u32).to_le_bytes());
        f.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        f.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        f.extend_from_slice(pkt);
    }
    f
}

/// Build a pcapng (LE) file: SHB, one IDB, then one EPB per record.
/// `tsresol` is written as `if_tsresol` when `Some`.
fn pcapng_raw_ip(records: &[(u64, Vec<u8>)], linktype: u16, tsresol: Option<u8>) -> Vec<u8> {
    let mut f = Vec::new();
    // Section Header Block.
    let mut shb = Vec::new();
    shb.extend_from_slice(&0x1a2b_3c4du32.to_le_bytes());
    shb.extend_from_slice(&1u16.to_le_bytes()); // major
    shb.extend_from_slice(&0u16.to_le_bytes()); // minor
    shb.extend_from_slice(&(-1i64).to_le_bytes()); // section length: unknown
    push_block(&mut f, 0x0a0d_0d0a, &shb);
    // Interface Description Block.
    let mut idb = Vec::new();
    idb.extend_from_slice(&linktype.to_le_bytes());
    idb.extend_from_slice(&0u16.to_le_bytes()); // reserved
    idb.extend_from_slice(&65535u32.to_le_bytes()); // snaplen
    if let Some(r) = tsresol {
        idb.extend_from_slice(&9u16.to_le_bytes()); // if_tsresol
        idb.extend_from_slice(&1u16.to_le_bytes());
        idb.extend_from_slice(&[r, 0, 0, 0]); // value + padding
        idb.extend_from_slice(&[0u8; 4]); // opt_endofopt
    }
    push_block(&mut f, 0x0000_0001, &idb);
    for (ticks, pkt) in records {
        let mut epb = Vec::new();
        epb.extend_from_slice(&0u32.to_le_bytes()); // interface id
        epb.extend_from_slice(&((ticks >> 32) as u32).to_le_bytes());
        epb.extend_from_slice(&(*ticks as u32).to_le_bytes());
        epb.extend_from_slice(&(pkt.len() as u32).to_le_bytes()); // captured
        epb.extend_from_slice(&(pkt.len() as u32).to_le_bytes()); // original
        epb.extend_from_slice(pkt);
        epb.resize(epb.len().div_ceil(4) * 4, 0); // packet data padding
        push_block(&mut f, 0x0000_0006, &epb);
    }
    f
}

/// A pcapng SECTION with `ifaces` interface descriptions, then one EPB per
/// `(iface_id, ticks, frame)` — a `mergecap` output in miniature.
fn pcapng_merged(ifaces: usize, records: &[(u32, u64, Vec<u8>)]) -> Vec<u8> {
    let mut f = Vec::new();
    let mut shb = Vec::new();
    shb.extend_from_slice(&0x1a2b_3c4du32.to_le_bytes());
    shb.extend_from_slice(&1u16.to_le_bytes());
    shb.extend_from_slice(&0u16.to_le_bytes());
    shb.extend_from_slice(&(-1i64).to_le_bytes());
    push_block(&mut f, 0x0a0d_0d0a, &shb);
    for _ in 0..ifaces {
        let mut idb = Vec::new();
        idb.extend_from_slice(&1u16.to_le_bytes()); // LINKTYPE_ETHERNET
        idb.extend_from_slice(&0u16.to_le_bytes());
        idb.extend_from_slice(&65535u32.to_le_bytes());
        push_block(&mut f, 0x0000_0001, &idb);
    }
    for (iface, ticks, pkt) in records {
        let mut epb = Vec::new();
        epb.extend_from_slice(&iface.to_le_bytes());
        epb.extend_from_slice(&((ticks >> 32) as u32).to_le_bytes());
        epb.extend_from_slice(&(*ticks as u32).to_le_bytes());
        epb.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        epb.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        epb.extend_from_slice(pkt);
        epb.resize(epb.len().div_ceil(4) * 4, 0);
        push_block(&mut f, 0x0000_0006, &epb);
    }
    f
}

/// Frame a pcapng block: type, total_length, body, total_length.
fn push_block(out: &mut Vec<u8>, btype: u32, body: &[u8]) {
    let total = (12 + body.len()) as u32;
    out.extend_from_slice(&btype.to_le_bytes());
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&total.to_le_bytes());
}

fn udp_packet(payload: &[u8], sport: u16, dport: u16) -> Vec<u8> {
    let mut u = Vec::new();
    u.extend_from_slice(&sport.to_be_bytes());
    u.extend_from_slice(&dport.to_be_bytes());
    u.extend_from_slice(&((payload.len() + 8) as u16).to_be_bytes());
    u.extend_from_slice(&[0, 0]); // checksum (unverified)
    u.extend_from_slice(payload);
    u
}

/// IPv4 header + payload slice, optionally a fragment.
fn ipv4(payload: &[u8], ident: u16, frag_off_bytes: usize, more: bool) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut p = vec![0u8; 20];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[4..6].copy_from_slice(&ident.to_be_bytes());
    let ff = ((frag_off_bytes / 8) as u16) | if more { 0x2000 } else { 0 };
    p[6..8].copy_from_slice(&ff.to_be_bytes());
    p[8] = 64;
    p[9] = 17; // UDP
    p[12..16].copy_from_slice(&[10, 0, 0, 1]);
    p[16..20].copy_from_slice(&[10, 0, 0, 2]);
    p.extend_from_slice(payload);
    p
}

/// Ethernet header with one 802.1Q VLAN tag, carrying IPv4 — the shape
/// the archived pcapng corpus uses.
fn eth_vlan(ip: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&[0x3c, 0xfd, 0xfe, 0x81, 0x7f, 0x68]); // dst
    f.extend_from_slice(&[0xe0, 0x2f, 0x6d, 0x4a, 0xa8, 0x11]); // src
    f.extend_from_slice(&0x8100u16.to_be_bytes()); // 802.1Q
    f.extend_from_slice(&0x0028u16.to_be_bytes()); // vid
    f.extend_from_slice(&0x0800u16.to_be_bytes()); // IPv4
    f.extend_from_slice(ip);
    f
}

fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("sip-pcap-test-{}-{name}", std::process::id()));
    std::fs::write(&p, bytes).unwrap();
    p
}

fn write_tmp_gz(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes).unwrap();
    write_tmp(name, &enc.finish().unwrap())
}

#[test]
fn decodes_a_plain_udp_datagram() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let pkt = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
    let file = write_tmp("plain", &pcap_raw_ip(&[(1_000_000, pkt)]));
    let (dgs, stats) = read_capture_files(&[&file]).unwrap();
    std::fs::remove_file(&file).ok();
    assert_eq!(dgs.len(), 1);
    assert_eq!(dgs[0].payload, msg);
    assert_eq!(dgs[0].src.port(), 5060);
    assert_eq!(dgs[0].dst.port(), 5080);
    assert_eq!(stats.datagrams, 1);
}

#[test]
fn reassembles_two_fragments_even_out_of_order() {
    // One UDP datagram split at an 8-byte-aligned boundary, delivered
    // second-fragment-first.
    let body: Vec<u8> = (0..900u32).map(|i| (i % 251) as u8).collect();
    let udp = udp_packet(&body, 6001, 5060);
    let cut = 600; // multiple of 8
    let f1 = ipv4(&udp[..cut], 7, 0, true);
    let f2 = ipv4(&udp[cut..], 7, cut, false);
    let file = write_tmp("frag", &pcap_raw_ip(&[(1_000_000, f2), (1_000_500, f1)]));
    let (dgs, stats) = read_capture_files(&[&file]).unwrap();
    std::fs::remove_file(&file).ok();
    assert_eq!(dgs.len(), 1, "stats: {stats}");
    assert_eq!(dgs[0].payload, body);
    assert_eq!(stats.reassembled, 1);
    assert_eq!(stats.frag_dropped, 0);
}

#[test]
fn missing_fragment_is_dropped_not_padded() {
    let body: Vec<u8> = vec![7u8; 900];
    let udp = udp_packet(&body, 6001, 5060);
    let f2 = ipv4(&udp[600..], 9, 600, false); // last fragment only
    let file = write_tmp("hole", &pcap_raw_ip(&[(1_000_000, f2)]));
    let (dgs, stats) = read_capture_files(&[&file]).unwrap();
    std::fs::remove_file(&file).ok();
    assert!(dgs.is_empty());
    assert_eq!(stats.frag_dropped, 1); // counted at EOF
}

#[test]
fn stale_pending_reassembly_expires_on_capture_time() {
    let body: Vec<u8> = vec![7u8; 900];
    let udp = udp_packet(&body, 6001, 5060);
    let f1 = ipv4(&udp[..600], 11, 0, true);
    // A later unrelated fragment 60s on (past REASSEMBLY_TTL_US) triggers expiry.
    let other = udp_packet(&[1u8; 16], 1, 2);
    let g1 = ipv4(&other[..8], 12, 0, true);
    let file = write_tmp("stale", &pcap_raw_ip(&[(1_000_000, f1), (61_000_000, g1)]));
    let (dgs, stats) = read_capture_files(&[&file]).unwrap();
    std::fs::remove_file(&file).ok();
    assert!(dgs.is_empty());
    // f1's entry expired (1) + g1 still pending at EOF (1).
    assert_eq!(stats.frag_dropped, 2);
}

/// A pcapng capture decodes to the same datagrams as the classic form,
/// through the Ethernet+VLAN link layer the archived corpus uses.
#[test]
fn decodes_a_pcapng_capture() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let frame = eth_vlan(&ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false));
    let file = write_tmp("ng", &pcapng_raw_ip(&[(1_638_412_000_000_000, frame)], 1, None));
    let (dgs, stats) = read_capture_files(&[&file]).unwrap();
    std::fs::remove_file(&file).ok();
    assert_eq!(dgs.len(), 1, "stats: {stats}");
    assert_eq!(dgs[0].payload, msg);
    // No if_tsresol option → pcapng's microsecond default, used verbatim.
    assert_eq!(dgs[0].ts_us, 1_638_412_000_000_000);
}

/// `if_tsresol` is honoured in both decimal and binary form: the same
/// wall-clock instant expressed in ns / µs / ms / 2^-32 s decodes alike.
#[test]
fn pcapng_timestamp_resolution_is_honoured() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let frame = eth_vlan(&ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false));
    let secs = 1_638_412_000u64;
    let cases = [
        (9u8, secs * 1_000_000_000), // nanoseconds
        (6u8, secs * 1_000_000),     // microseconds
        (3u8, secs * 1_000),         // milliseconds
        (0x80 | 32, secs << 32),     // binary, 2^-32 s
    ];
    for (resol, ticks) in cases {
        let file = write_tmp(
            &format!("ngres{resol}"),
            &pcapng_raw_ip(&[(ticks, frame.clone())], 1, Some(resol)),
        );
        let (dgs, _) = read_capture_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert_eq!(dgs.len(), 1, "resol {resol:#x}");
        assert_eq!(dgs[0].ts_us, secs * 1_000_000, "resol {resol:#x}");
    }
}

/// Compression is detected from the stream, not the file name: the same
/// bytes gzipped decode identically, for both containers.
#[test]
fn gzipped_captures_decode_identically() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
    let classic = pcap_raw_ip(&[(1_000_000, ip.clone())]);
    let ng = pcapng_raw_ip(&[(1_000_000, eth_vlan(&ip))], 1, None);
    for (name, bytes) in [("gzclassic", classic), ("gzng", ng)] {
        let plain = write_tmp(name, &bytes);
        let gz = write_tmp_gz(&format!("{name}z"), &bytes);
        let (a, _) = read_capture_files(&[&plain]).unwrap();
        let (b, _) = read_capture_files(&[&gz]).unwrap();
        std::fs::remove_file(&plain).ok();
        std::fs::remove_file(&gz).ok();
        assert_eq!(a.len(), 1, "{name}");
        assert_eq!(a[0].payload, b[0].payload, "{name}");
        assert_eq!(a[0].ts_us, b[0].ts_us, "{name}");
    }
}

/// A corpus mixing both containers and both compressions reads in one
/// call — the format is per-file, never a mode the caller must declare.
#[test]
fn mixed_container_corpus_reads_in_one_call() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
    let a = write_tmp("mixa", &pcap_raw_ip(&[(1_000_000, ip.clone())]));
    let b = write_tmp_gz("mixb", &pcapng_raw_ip(&[(2_000_000, eth_vlan(&ip))], 1, None));
    let (dgs, stats) = read_capture_files(&[&a, &b]).unwrap();
    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
    assert_eq!(dgs.len(), 2, "stats: {stats}");
    assert_eq!(stats.records, 2);
    assert_eq!(dgs[0].ts_us, 1_000_000);
    assert_eq!(dgs[1].ts_us, 2_000_000);
}

/// One capture split across ring files: each file begins where the last
/// ended, so the set reads as one stream from one observation point, and
/// a file with no datagram says nothing about time.
#[test]
fn a_consecutive_set_reads_as_one_capture() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
    let a = write_tmp("seta", &pcap_raw_ip(&[(1_000_000, ip.clone()), (2_000_000, ip.clone())]));
    let empty = write_tmp("setempty", &pcap_raw_ip(&[]));
    let b = write_tmp("setb", &pcap_raw_ip(&[(2_500_000, ip.clone()), (3_000_000, ip)]));
    let read = read_capture_set(&[&a, &empty, &b], 1_000_000);
    for f in [&a, &empty, &b] {
        std::fs::remove_file(f).ok();
    }
    let (dgs, stats) = read.unwrap();
    assert_eq!(stats.records, 4);
    assert_eq!(
        dgs.iter().map(|d| d.ts_us).collect::<Vec<_>>(),
        [1_000_000, 2_000_000, 2_500_000, 3_000_000]
    );
    // One tap: the second file's observation point is the first file's.
    assert!(dgs.iter().all(|d| d.probe == 0), "{dgs:?}");
}

/// A fragment split across two ring files is still reassembled: the set is
/// read with one reassembler, in the given order.
#[test]
fn a_consecutive_set_reassembles_across_the_boundary() {
    let payload = vec![b'A'; 1_000];
    let udp = udp_packet(&payload, 5060, 5080);
    let first = ipv4(&udp[..512], 7, 0, true);
    let second = ipv4(&udp[512..], 7, 512, false);
    let a = write_tmp("fraga", &pcap_raw_ip(&[(1_000_000, first)]));
    let b = write_tmp("fragb", &pcap_raw_ip(&[(1_000_100, second)]));
    let read = read_capture_set(&[&a, &b], 1_000_000);
    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
    let (dgs, stats) = read.unwrap();
    assert_eq!(stats.reassembled, 1, "stats: {stats}");
    assert_eq!(dgs.len(), 1);
    assert_eq!(dgs[0].payload, payload);
}

/// Files that are not consecutive slices of one tap are refused, naming
/// the two files and what lies between them: a gap wider than the
/// tolerance, an overlap, or a later file given first. An overlap inside
/// the boundary slack is a writer's reorder at the rotation, not two
/// captures of one wire.
#[test]
fn a_set_that_is_not_consecutive_is_refused() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
    let at = |name: &str, ts: &[u64]| {
        write_tmp(name, &pcap_raw_ip(&ts.iter().map(|&t| (t, ip.clone())).collect::<Vec<_>>()))
    };
    let a = at("nca", &[1_000_000, 2_000_000]);
    let far = at("ncfar", &[5_000_000]);
    let over = at("ncover", &[1_500_000, 2_500_000]);
    let slack = at("ncslack", &[2_000_000 - BOUNDARY_SLACK_US, 2_500_000]);
    let earlier = at("ncearlier", &[500_000]);

    let gap = read_capture_set(&[&a, &far], 1_000_000).unwrap_err().to_string();
    assert!(gap.contains("gap") && gap.contains("3.000000 s"), "{gap}");
    assert!(gap.contains("nca") && gap.contains("ncfar"), "{gap}");
    // The same gap inside a wider tolerance is consecutive.
    assert!(read_capture_set(&[&a, &far], 3_000_000).is_ok());

    let overlap = read_capture_set(&[&a, &over], 1_000_000).unwrap_err().to_string();
    assert!(overlap.contains("overlap") && overlap.contains("0.500000 s"), "{overlap}");

    assert!(read_capture_set(&[&a, &slack], 1_000_000).is_ok());

    let order = read_capture_set(&[&a, &earlier], 1_000_000).unwrap_err().to_string();
    assert!(order.contains("oldest first"), "{order}");

    for f in [&a, &far, &over, &slack, &earlier] {
        std::fs::remove_file(f).ok();
    }
}

/// A `mergecap` of two probes: the SAME packet, written once per probe, is
/// two datagrams identical in every field but the probe that wrote them.
/// Nothing else in the model can tell the two copies apart.
#[test]
fn each_pcapng_interface_is_its_own_probe() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let frame = eth_vlan(&ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false));
    // Probe 1's clock runs 8 s ahead of probe 0's — far outside any
    // capture-dedup window, and squarely on the INVITE retransmit ladder.
    let file = write_tmp(
        "twoprobes",
        &pcapng_merged(2, &[(0, 1_000_000, frame.clone()), (1, 9_000_000, frame)]),
    );
    let (dgs, _) = read_capture_files(&[&file]).unwrap();
    std::fs::remove_file(&file).ok();
    assert_eq!(dgs.len(), 2);
    assert_eq!(dgs[0].payload, dgs[1].payload, "one packet, two records");
    assert_eq!((dgs[0].probe, dgs[1].probe), (0, 1), "one probe per interface");
}

/// Probes are numbered across the WHOLE read, so two files' interface 0 are
/// two observation points and never one.
#[test]
fn probes_are_numbered_across_every_file_read() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
    let a = write_tmp("pa", &pcap_raw_ip(&[(1_000_000, ip.clone())]));
    let b = write_tmp("pb", &pcapng_merged(2, &[(0, 2_000_000, eth_vlan(&ip))]));
    let (dgs, _) = read_capture_files(&[&a, &b]).unwrap();
    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
    assert_eq!(dgs.iter().map(|d| d.probe).collect::<Vec<_>>(), vec![0, 1]);
}

/// A truncated tail (the capture was still being written) is counted, not
/// an error: the datagrams before the cut are still returned.
#[test]
fn truncated_pcapng_tail_is_counted_not_fatal() {
    let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
    let frame = eth_vlan(&ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false));
    let mut bytes = pcapng_raw_ip(&[(1_000_000, frame.clone()), (2_000_000, frame)], 1, None);
    bytes.truncate(bytes.len() - 40); // cut into the second EPB
    let file = write_tmp("ngtrunc", &bytes);
    let (dgs, stats) = read_capture_files(&[&file]).unwrap();
    std::fs::remove_file(&file).ok();
    assert_eq!(dgs.len(), 1);
    assert_eq!(stats.tail_truncated, 1);
}

#[test]
fn unknown_container_is_a_clear_error() {
    let file = write_tmp("junk", b"not a capture at all, just bytes");
    let err = read_capture_files(&[&file]).unwrap_err();
    std::fs::remove_file(&file).ok();
    assert!(format!("{err}").contains("not a pcap or pcapng capture"), "unhelpful error: {err}");
}
