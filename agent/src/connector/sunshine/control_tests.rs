//! Control stream tests: the encryption against vectors made with another
//! AES-GCM (layout from ControlStream.c and stream.cpp), and the running
//! stream against the ENet control server of `fake_host.rs` (Sunshine's
//! stream.cpp), which decrypts with its own code.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use super::super::fake_host::{ControlSeen, FakeHost, DESKTOP_ID};
use super::super::http::{hex_decode, hex_encode, HostClient, LaunchParams};
use super::super::net::Reachability;
use super::super::rtsp::{self, RtspClient, StreamRequest};
use super::super::video::{AppVersion, VideoCodec};
use super::*;

const KEY: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

#[test]
fn messages_match_the_vectors_in_both_directions() {
    // Expected bytes from Python `cryptography` (OpenSSL) AESGCM with key
    // 00..0f, laid out as ControlStream.c encryptControlMessage does:
    // LE16 0x0001, LE16 length, LE32 seq, tag, ciphertext of (LE16 type,
    // LE16 payload length, payload).
    let v2 = ControlCipher::new(KEY, true);
    // Client, seq 0: the IDR request {0, 0}. IV 00000000 .. 'C' 'C'.
    let idr = "01001a000000000022302723537811166cb9bfead7dad4575cd629f5cafe";
    assert_eq!(hex_encode(&v2.seal(0, false, PTYPE_REQUEST_IDR_FRAME, &[0, 0])), idr);
    // Client, seq 0x01020304: the periodic ping. IV 04030201 ..
    let ping = "0100200004030201e26fd2aa218448bbf09f4a06654b13fb10de601bb04f1b63397987e6";
    assert_eq!(hex_encode(&v2.seal(0x0102_0304, false, PTYPE_PERIODIC_PING, &[4, 0, 0, 0, 0, 0, 0, 0])), ping);
    // Host, seq 7: termination 0x80030023 (stream.cpp encode_control). IV .. 'H' 'C'.
    let termination = "01001c0007000000a825d6bd20ccfe5ba7fbb60530900ef17f27937ed176265a";
    assert_eq!(
        v2.open(true, &hex_decode(termination).unwrap()),
        Ok((7, PTYPE_TERMINATION, vec![0x80, 0x03, 0x00, 0x23]))
    );
    assert_eq!(hex_encode(&v2.seal(7, true, PTYPE_TERMINATION, &[0x80, 0x03, 0x00, 0x23])), termination);
    // What this side sends opens as the host opens it, and not the other way round.
    assert_eq!(v2.open(false, &hex_decode(idr).unwrap()), Ok((0, PTYPE_REQUEST_IDR_FRAME, vec![0, 0])));
    assert_eq!(v2.open(true, &hex_decode(idr).unwrap()), Err(CipherError::Authentication));

    // Without SS_ENC_CONTROL_V2: NVIDIA's 16-byte IV whose first byte is the
    // sequence number cut to a byte (0x1ff -> 0xff), the same for both sides.
    let legacy = ControlCipher::new(KEY, false);
    let idr = "01001a00ff01000005693743f0f1a462cba42efc2a18722997c56369ad73";
    assert_eq!(hex_encode(&legacy.seal(0x1ff, false, PTYPE_REQUEST_IDR_FRAME, &[0, 0])), idr);
    let termination = "01001c0003000000ed1c7c362ed5a106758eb68707fb95c5256bd16ed5c46999";
    assert_eq!(
        legacy.open(true, &hex_decode(termination).unwrap()),
        Ok((3, PTYPE_TERMINATION, vec![0x80, 0x03, 0x00, 0x23]))
    );
    assert_eq!(v2.open(true, &hex_decode(termination).unwrap()), Err(CipherError::Authentication));
}

#[test]
fn open_refuses_what_is_not_right() {
    let cipher = ControlCipher::new(KEY, true);
    let good = cipher.seal(5, true, PTYPE_RUMBLE_DATA, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(cipher.open(true, &good), Ok((5, PTYPE_RUMBLE_DATA, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10])));
    // Sequence number, tag or text changed.
    for at in [4, 8, 23, 24, good.len() - 1] {
        let mut bad = good.clone();
        bad[at] ^= 0x80;
        assert_eq!(cipher.open(true, &bad), Err(CipherError::Authentication), "byte {at}");
    }
    assert_eq!(ControlCipher::new([7; 16], true).open(true, &good), Err(CipherError::Authentication));
    // A plain message (type 0x0302) on the encrypted stream.
    assert_eq!(cipher.open(true, &[0x02, 0x03, 0, 0]), Err(CipherError::NotEncrypted));
    // Runts and lengths that lie.
    assert_eq!(cipher.open(true, &[]), Err(CipherError::Malformed));
    assert_eq!(cipher.open(true, &[1]), Err(CipherError::Malformed));
    assert_eq!(cipher.open(true, &good[..7]), Err(CipherError::Malformed));
    assert_eq!(cipher.open(true, &good[..good.len() - 1]), Err(CipherError::Malformed));
    let mut short = good.clone();
    short[2] = 23; // less than seq + tag + inner header
    assert_eq!(cipher.open(true, &short), Err(CipherError::Malformed));
    // A message with an empty payload.
    let empty = cipher.seal(6, true, PTYPE_HDR_MODE, &[]);
    assert_eq!(cipher.open(true, &empty), Ok((6, PTYPE_HDR_MODE, vec![])));
    assert_eq!(empty.len(), 8 + 16 + 4);
}

#[test]
fn termination_reasons_in_words() {
    assert_eq!(termination_text(TERMINATION_CLOSED, true), "The session was ended at the other computer");
    assert!(termination_text(TERMINATION_CLOSED, false).starts_with("The other computer ended the session before it sent a picture"));
    assert_eq!(termination_text(TERMINATION_SHORT_INTENDED, true), "The session was ended at the other computer");
    assert!(termination_text(TERMINATION_PROTECTED_CONTENT, true).contains("protected content"));
    assert!(termination_text(TERMINATION_FRAME_CONVERSION, true).contains("could not prepare its picture"));
    assert_eq!(termination_text(0x8003_0001, true), "The other computer ended the session (reason 0x80030001)");
}

// ---------------------------------------------------------------------------
// Against the host
// ---------------------------------------------------------------------------

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// A launch and the RTSP exchange; what the control stream needs afterwards.
/// `v2`: whether the SDP enables SS_ENC_CONTROL_V2 (an old host offers none).
async fn session_at(host: &FakeHost, v2: bool) -> ControlParams {
    let (identity, server_cert) = host.pair_directly("00000000000000C2");
    let client = HostClient::new("127.0.0.1", host.http_port, &identity, Some(&server_cert)).unwrap();
    let info = client.server_info().await.unwrap();
    let params = LaunchParams::new(DESKTOP_ID, 1280, 720, 60).unwrap();
    let answer = client.start_app(&info, &params).await.unwrap();
    let mut rtsp = RtspClient::new(answer.session_url.as_deref(), LOCALHOST, &params.rikey, AppVersion::SUNSHINE, true);
    rtsp.options().await.unwrap();
    let mut describe = rtsp.describe().await.unwrap();
    if !v2 {
        describe.encryption_supported = 0;
        describe.encryption_requested = 0;
    }
    rtsp.setup_audio().await.unwrap();
    let (video_port, _) = rtsp.setup_video().await.unwrap();
    let (control_port, connect_data) = rtsp.setup_control().await.unwrap();
    let request = StreamRequest { codec: VideoCodec::H264, width: 1280, height: 720, fps: 60, bitrate_kbps: 20000, encryption_flags: rtsp::ENCFLG_ALL };
    let (sdp, negotiated) = rtsp::build_sdp(&request, &describe, AppVersion::SUNSHINE, &LOCALHOST, Reachability::Unknown, video_port);
    assert_eq!(negotiated.encryption_enabled & rtsp::SS_ENC_CONTROL_V2 != 0, v2);
    rtsp.announce(&sdp).await.unwrap();
    rtsp.play().await.unwrap();
    ControlParams { remote: SocketAddr::new(LOCALHOST, control_port), connect_data, key: params.rikey, encryption_v2: v2 }
}

fn of_type(seen: &[ControlSeen], ptype: u16) -> Vec<&ControlSeen> {
    seen.iter().filter(|m| m.ptype == ptype).collect()
}

async fn a_whole_control_stream(v2: bool) {
    let host = FakeHost::start().await;
    let params = session_at(&host, v2).await;
    let mut control = Control::connect(params).await.unwrap();

    // startControlStream: "start A" (the IDR request at that place) and
    // "start B" on the generic channel; then a ping every 100 ms.
    host.wait("start B and three pings", |st| st.stream.start_b == 1 && st.stream.periodic_pings >= 3).await;
    {
        let st = host.state.lock().unwrap();
        assert_eq!(st.stream.control_connects, 1);
        // "Once the control stream connection is established, RTSP session state can be torn down"
        assert!(st.stream.launch.is_none());
        let seen = &st.stream.control;
        assert_eq!(seen[0], ControlSeen { channel: 0, ptype: 0x0302, payload: vec![0, 0], seq: 0 });
        assert_eq!(seen[1], ControlSeen { channel: 0, ptype: 0x0307, payload: vec![0], seq: 1 });
        for ping in of_type(seen, 0x0200) {
            assert_eq!((ping.channel, ping.payload.as_slice()), (0, &[4u8, 0, 0, 0, 0, 0, 0, 0][..]));
        }
        // currentEnetSequenceNumber: one number per message, in order.
        for (index, message) in seen.iter().enumerate() {
            assert_eq!(message.seq, index as u32);
        }
        assert_eq!(st.stream.idr_requests, 1);
        assert_eq!(st.stream.control_rejected, 0);
    }
    // The pings come at their pace: about ten a second.
    let before = host.state.lock().unwrap().stream.periodic_pings;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = host.state.lock().unwrap().stream.periodic_pings;
    assert!((3..=7).contains(&(after - before)), "{} pings in half a second", after - before);

    // requestIdrFrame: the urgent channel.
    control.request_idr();
    host.wait("the IDR request", |st| st.stream.idr_requests == 2).await;
    // The FEC status goes with the next ping, for the host's log.
    let status: [u8; 21] = std::array::from_fn(|i| i as u8 + 1);
    control.send_fec_status(status);
    host.wait("the FEC status", |st| st.stream.fec_status.len() == 1).await;
    // Input: the packet as it is, on its channel.
    let rel_move = [0, 0, 0, 8, 7, 0, 0, 0, 0, 5, 0xFF, 0xFD];
    control.send_input(CTRL_CHANNEL_MOUSE, rel_move.to_vec());
    host.wait("the input message", |st| !st.stream.input.is_empty()).await;
    {
        let st = host.state.lock().unwrap();
        let seen = &st.stream.control;
        let idr = of_type(seen, 0x0302)[1];
        assert_eq!((idr.channel, idr.payload.as_slice()), (CTRL_CHANNEL_URGENT, &[0u8, 0][..]));
        let fec = of_type(seen, 0x5502)[0];
        assert_eq!((fec.channel, fec.payload.as_slice()), (0, &status[..]));
        let input = of_type(seen, 0x0206)[0];
        assert_eq!((input.channel, input.payload.as_slice()), (CTRL_CHANNEL_MOUSE, &rel_move[..]));
        assert_eq!(st.stream.malformed_input, 0);
    }

    // What a host may send and nothing here uses: rumble, HDR mode, a
    // controller extension. They are opened, counted and left alone.
    host.send_control(0x010b, &[0, 0, 0, 0, 1, 0, 0x34, 0x12, 0x78, 0x56]);
    host.send_control(0x010e, &[1, 0, 0]);
    host.send_control(0x5500, &[0, 0, 1, 0, 2, 0]);
    for _ in 0..400 {
        if control.stats().received == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let stats = control.stats();
    assert_eq!((stats.received, stats.rejected), (3, 0));
    assert_eq!((stats.idr_requests_sent, stats.fec_status_sent, stats.input_messages_sent), (1, 1, 1));
    assert!(stats.pings_sent >= 8);

    // The host's termination ends the stream with its reason.
    host.send_control(0x0109, &0x8003_0023u32.to_be_bytes());
    let event = tokio::time::timeout(Duration::from_secs(5), control.event()).await.unwrap();
    assert_eq!(event, Some(ControlEvent::Terminated { code: TERMINATION_CLOSED }));
    // Nothing follows.
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), control.event()).await.unwrap(), None);
    let stats = control.close().await;
    assert_eq!(stats.received, 4);
    // The round trip of the acknowledged pings on the loopback, not ENet's
    // starting value of 500 ms (to which it falls back when the peer is
    // reset at the disconnect: what the first run against a real host showed).
    assert!(stats.round_trip_ms < 400, "{} ms", stats.round_trip_ms);
}

#[tokio::test]
async fn a_control_stream_with_the_new_encryption() {
    a_whole_control_stream(true).await;
}

#[tokio::test]
async fn a_control_stream_with_the_old_encryption() {
    a_whole_control_stream(false).await;
}

#[tokio::test]
async fn closing_says_goodbye_and_dropping_too() {
    let host = FakeHost::start().await;
    let params = session_at(&host, true).await;
    let control = Control::connect(params).await.unwrap();
    host.wait("the control connection", |st| st.stream.control_connects == 1).await;
    // An input message right before the end still arrives (the linger of
    // stopControlStream: "including any key up events that might be sent").
    control.send_input(CTRL_CHANNEL_KEYBOARD, vec![0, 0, 0, 10, 4, 0, 0, 0, 0, 0x41, 0x80, 0, 0, 0]);
    let started = std::time::Instant::now();
    let stats = control.close().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(stats.round_trip_ms < 500, "{} ms", stats.round_trip_ms);
    {
        let st = host.state.lock().unwrap();
        assert_eq!(st.stream.control_disconnects, 1);
        // "CLIENT DISCONNECTED": the session stopped.
        assert!(st.stream.session.is_none());
        assert_eq!(st.stream.input.len(), 1);
    }

    let host = FakeHost::start().await;
    let params = session_at(&host, true).await;
    let control = Control::connect(params).await.unwrap();
    host.wait("the control connection", |st| st.stream.control_connects == 1).await;
    drop(control);
    host.wait("the disconnect after a drop", |st| st.stream.control_disconnects == 1).await;
}

#[tokio::test]
async fn a_host_that_does_not_know_the_connection_refuses_it() {
    // Wrong connect data: "Rejected connection ...: it's not properly set up".
    // ENet itself connects (the host learns the number only with the
    // connection); then the host drops the peer, which is the end.
    let host = FakeHost::start().await;
    let mut params = session_at(&host, true).await;
    params.connect_data ^= 0x10;
    let mut control = Control::connect(params).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), control.event()).await.unwrap();
    assert_eq!(event, Some(ControlEvent::Lost));
    let st = host.state.lock().unwrap();
    assert_eq!((st.stream.control_refused, st.stream.control_connects), (1, 0));
    assert!(st.stream.control.is_empty());
}

#[tokio::test]
async fn a_wrong_key_reaches_nothing() {
    // The host cannot read a single message (and a real one stops the session).
    let host = FakeHost::start().await;
    let mut params = session_at(&host, true).await;
    params.key = [9; 16];
    let control = Control::connect(params).await.unwrap();
    host.wait("rejected messages", |st| st.stream.control_rejected >= 3).await;
    assert!(host.state.lock().unwrap().stream.control.is_empty());
    // And what the host sends is not taken either.
    host.send_control(0x0109, &0x8003_0023u32.to_be_bytes());
    for _ in 0..400 {
        if control.stats().rejected == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!((control.stats().rejected, control.stats().received), (1, 0));
}

#[tokio::test]
async fn the_reason_is_read_even_when_the_host_hangs_up_right_after_it() {
    // Sunshine at the end of its control thread: the termination message,
    // a flush, and the peers are dropped. Both datagrams wait in the socket
    // when this side looks; stock ENet would throw the message away with the
    // disconnect (ControlStream.c ignoreDisconnectIntercept is there for that).
    for _ in 0..5 {
        let host = FakeHost::start().await;
        let params = session_at(&host, true).await;
        let mut control = Control::connect(params).await.unwrap();
        host.wait("the control connection", |st| st.stream.start_b == 1).await;
        {
            let mut st = host.state.lock().unwrap();
            st.stream.session.as_mut().unwrap().stop();
            drop(st);
            host.send_control(0x0109, &0x8003_0023u32.to_be_bytes());
        }
        let event = tokio::time::timeout(Duration::from_secs(5), control.event()).await.unwrap();
        assert_eq!(event, Some(ControlEvent::Terminated { code: TERMINATION_CLOSED }));
    }
}

#[test]
fn a_lone_disconnect_is_told_from_other_datagrams() {
    // ENet: peerID (0x8000: a sentTime follows), sentTime; command 4 =
    // DISCONNECT (with or without the acknowledge flag 0x80), channel 0xFF,
    // sequence number, data.
    assert!(is_lone_disconnect(&[0x80, 0x00, 0x12, 0x34, 0x84, 0xFF, 0x00, 0x05, 0, 0, 0, 0]));
    assert!(is_lone_disconnect(&[0x00, 0x00, 0x04, 0xFF, 0x00, 0x00, 0, 0, 0, 0]));
    // A ping (command 5), a disconnect with something after it, a runt.
    assert!(!is_lone_disconnect(&[0x80, 0x00, 0x12, 0x34, 0x85, 0xFF, 0x00, 0x05]));
    assert!(!is_lone_disconnect(&[0x80, 0x00, 0x12, 0x34, 0x84, 0xFF, 0x00, 0x05, 0, 0, 0, 0, 0x85, 0xFF, 0, 6]));
    assert!(!is_lone_disconnect(&[0x80]));
    assert!(!is_lone_disconnect(&[]));
}

/// The measured round trip becomes the delay the viewer adds to its ping.
#[test]
fn the_round_trip_is_told_in_tenths_of_a_millisecond_when_it_changes() {
    let stats = |round_trip_us| ControlStats { round_trip_us, ..Default::default() };
    // Nothing measured yet.
    assert_eq!(round_trip_to_tell(&stats(0), None), None);
    // 0.42 ms -> 4 tenths; 9.36 ms -> 94.
    assert_eq!(round_trip_to_tell(&stats(420), None), Some(4));
    assert_eq!(round_trip_to_tell(&stats(9_360), None), Some(94));
    // The same to the tenth says nothing; a change does.
    assert_eq!(round_trip_to_tell(&stats(9_380), Some(94)), None);
    assert_eq!(round_trip_to_tell(&stats(14_000), Some(94)), Some(140));
}

/// An ENet datagram of this side: the header with `sentTime`, then one
/// reliable PING command on channel 0xFF.
fn enet_ping(sent_time: u16, seq: u16) -> Vec<u8> {
    let mut d = vec![0x80, 0x00];
    d.extend(sent_time.to_be_bytes());
    d.extend([0x80 | 5, 0xFF]);
    d.extend(seq.to_be_bytes());
    d
}

/// The host's ACKNOWLEDGE of a command sent at `sent_time`.
fn enet_ack(seq: u16, sent_time: u16) -> Vec<u8> {
    let mut c = vec![1, 0xFF, 0, 0];
    c.extend(seq.to_be_bytes());
    c.extend(sent_time.to_be_bytes());
    c
}

#[test]
fn round_trips_are_measured_from_a_datagram_to_its_acknowledgement() {
    let start = Instant::now();
    let at = |micros: u64| start + Duration::from_micros(micros);
    let mut trips = RoundTrips::default();
    assert_eq!(trips.median_us(at(0)), None);

    // A ping leaves at 1000 us with ENet's time 500; its acknowledgement
    // comes 730 us later, alone in a datagram without a time of its own.
    trips.sent(&enet_ping(500, 1), at(1_000));
    let mut datagram = vec![0x00, 0x00];
    datagram.extend(enet_ack(1, 500));
    trips.received(&datagram, at(1_730));
    assert_eq!(trips.median_us(at(2_000)), Some(730));

    // An acknowledgement behind other commands in a datagram that has a
    // time: a reliable message of 5 bytes, an unsequenced one of 3, a ping.
    trips.sent(&enet_ping(600, 2), at(101_000));
    let mut datagram = vec![0x80, 0x00, 0x12, 0x34];
    datagram.extend([0x80 | 6, 0, 0, 9, 0, 5, 1, 2, 3, 4, 5]);
    datagram.extend([0x40 | 9, 0, 0, 0, 0, 1, 0, 3, 7, 7, 7]);
    datagram.extend([0x80 | 5, 0xFF, 0, 3]);
    datagram.extend(enet_ack(2, 600));
    trips.received(&datagram, at(101_910));
    // Two samples (730, 910): the upper of the two middle ones.
    assert_eq!(trips.median_us(at(102_000)), Some(910));

    // The same acknowledgement again (two commands of one datagram are
    // acknowledged with the same time): one sample only.
    let mut again = vec![0x00, 0x00];
    again.extend(enet_ack(2, 600));
    again.extend(enet_ack(3, 600));
    trips.received(&again, at(150_000));
    assert_eq!(trips.samples.len(), 2);

    // A time nobody sent, a datagram that is cut off, one that is compressed.
    let mut unknown = vec![0x00, 0x00];
    unknown.extend(enet_ack(4, 999));
    trips.received(&unknown, at(160_000));
    trips.received(&[0x00, 0x00, 1, 0xFF, 0], at(160_000));
    let mut compressed = vec![0x40, 0x00];
    compressed.extend(enet_ack(1, 500));
    trips.received(&compressed, at(160_000));
    trips.received(&[], at(160_000));
    assert_eq!(trips.samples.len(), 2);

    // One late acknowledgement among quick ones does not move the figure.
    for (i, micros) in [800u64, 820, 790, 45_000, 810].into_iter().enumerate() {
        let sent_time = 700 + i as u16;
        trips.sent(&enet_ping(sent_time, 10 + i as u16), at(200_000 + i as u64 * 100_000));
        let mut datagram = vec![0x00, 0x00];
        datagram.extend(enet_ack(10 + i as u16, sent_time));
        trips.received(&datagram, at(200_000 + i as u64 * 100_000 + micros));
    }
    assert_eq!(trips.median_us(at(700_000)), Some(810));
    // Only the last second counts; after it, nothing is known.
    assert_eq!(trips.median_us(at(1_700_000)), None);

    // A datagram without ENet's time (no reliable command in it) is not kept.
    trips.sent(&[0x00, 0x00, 7, 0, 0, 0, 0, 0, 0, 0], at(6_000_000));
    assert!(trips.sent.iter().all(|(sent_time, _, _)| *sent_time != 0x0700));
}
