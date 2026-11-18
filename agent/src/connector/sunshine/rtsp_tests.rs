//! RTSP tests: vectors for the encryption (made with another AES-GCM, the
//! layout from RtspConnection.c), the texts byte for byte, and the whole
//! exchange against the host of `fake_host.rs` (Sunshine's rtsp.cpp), plain
//! and encrypted.

use std::net::{IpAddr, Ipv4Addr};

use super::super::fake_host::{EncryptionMode, FakeHost, DESKTOP_ID};
use super::super::http::{hex_decode, hex_encode, HostClient, LaunchParams};
use super::*;

const KEY: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

const OPTIONS: &str = "OPTIONS rtsp://10.0.0.2:48010 RTSP/1.0\r\nCSeq: 1\r\nX-GS-ClientVersion: 14\r\nHost: 10.0.0.2\r\n\r\n";

#[test]
fn sealed_messages_match_the_vectors() {
    // Expected bytes from Python `cryptography` (OpenSSL) AESGCM with key
    // 00..0f, laid out as RtspConnection.c sealRtspMessage does: BE32
    // 0x80000000|length, BE32 sequence number, tag, ciphertext; IV = sequence
    // number little-endian, zeros, 'C' 'R'.
    let expected = "8000005b00000001857573ab090ee3c10ed0e2766e91c5529fe452003a42126629e9a38daca4f662beaadbf24fb40a6b9f048d30919fe7fd5b40e4cc1b354153fbcf05db96c81716f2d6d2e60f534c9ee6b080ed529938932cb06fc5a34d42c40c15478408264cfe9059557356828678be77b5";
    assert_eq!(hex_encode(&seal(&KEY, 1, false, OPTIONS.as_bytes())), expected);
    assert_eq!(unseal(&KEY, false, &hex_decode(expected).unwrap()).unwrap(), (1, OPTIONS.as_bytes().to_vec()));

    // The host's answer (rtsp.cpp respond): the same with 'H' 'R'.
    let answer = "8000001c00000001f1734c68fca6356daf5717bc54c6f6a2600872b43a271f6b4fc685cedf9a7cb84351077c0cc521456e1d06e9";
    let raw = hex_decode(answer).unwrap();
    let (seq, plain) = unseal(&KEY, true, &raw).unwrap();
    assert_eq!(seq, 1);
    assert_eq!(plain, b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n");
    assert_eq!(hex_encode(&seal(&KEY, 1, true, &plain)), answer);
}

#[test]
fn unseal_refuses_what_is_not_right() {
    let good = seal(&KEY, 7, true, b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n");
    assert!(unseal(&KEY, true, &good).is_ok());
    // The other direction's IV.
    assert!(unseal(&KEY, false, &good).is_err());
    // Another key.
    assert!(unseal(&[9; 16], true, &good).is_err());
    // Any changed byte: length, sequence number, tag, text.
    for at in [3, 7, 8, 23, 24, good.len() - 1] {
        let mut bad = good.clone();
        bad[at] ^= 1;
        assert!(unseal(&KEY, true, &bad).is_err(), "byte {at}");
    }
    // "Rejecting unencrypted RTSP message", partial, excess, header only.
    assert!(unseal(&KEY, true, b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n").is_err());
    assert!(unseal(&KEY, true, &good[..good.len() - 1]).is_err());
    let mut longer = good.clone();
    longer.push(0);
    assert!(unseal(&KEY, true, &longer).is_err());
    assert!(unseal(&KEY, true, &good[..24]).is_err());
}

#[test]
fn requests_and_answers_as_text() {
    // RtspParser.c serializeRtspMessage: "command target protocol\r\n", "option: content\r\n", "\r\n", payload.
    let request = serialize_request(
        "OPTIONS",
        "rtsp://10.0.0.2:48010",
        &[("CSeq", "1".into()), ("X-GS-ClientVersion", "14".into()), ("Host", "10.0.0.2".into())],
        &[],
    );
    assert_eq!(request, OPTIONS.as_bytes());
    let with_payload = serialize_request("ANNOUNCE", "streamid=control/13/0", &[("Content-length", "3".into())], b"v=0");
    assert_eq!(with_payload, b"ANNOUNCE streamid=control/13/0 RTSP/1.0\r\nContent-length: 3\r\n\r\nv=0");

    // An answer of rtsp.cpp cmd_setup.
    let answer = parse_response(
        b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: DEADBEEFCAFE;timeout = 90\r\nTransport: server_port=48000\r\nX-SS-Ping-Payload: 0123456789ABCDEF\r\n\r\n",
    )
    .unwrap();
    assert_eq!(answer.status, 200);
    assert_eq!(answer.status_text, "OK");
    assert_eq!(answer.header("session"), Some("DEADBEEFCAFE;timeout = 90"));
    assert_eq!(server_port(&answer), Some(48000));
    assert!(answer.payload.is_empty());
    // With a payload whose lines end in "\n" only (cmd_describe uses std::endl).
    let answer = parse_response(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\na=one:1\na=two:2\n").unwrap();
    assert_eq!(answer.payload, b"a=one:1\na=two:2\n");
    let refused = parse_response(b"RTSP/1.0 404 NOT FOUND\r\n\r\n").unwrap();
    assert_eq!((refused.status, refused.status_text.as_str()), (404, "NOT FOUND"));
    assert!(parse_response(b"HTTP/1.1 200 OK\r\n\r\n").is_err());
    assert!(parse_response(b"RTSP/1.0 abc\r\n\r\n").is_err());
}

#[test]
fn describe_of_a_sunshine_host() {
    // As rtsp.cpp cmd_describe writes it on a private network with default settings.
    let payload = "a=x-ss-general.featureFlags:3\na=x-ss-general.encryptionSupported:5\na=x-ss-general.encryptionRequested:1\na=x-nv-video[0].refPicInvalidation:1\nsprop-parameter-sets=AAAAAU\na=fmtp:97 surround-params=21101\n";
    assert_eq!(
        parse_describe(payload),
        Describe {
            hevc: true,
            av1: false,
            reference_frame_invalidation: true,
            feature_flags: 3,
            encryption_supported: 5,
            encryption_requested: 1,
        }
    );
    let d = parse_describe("a=x-ss-general.encryptionSupported:7\r\na=x-ss-general.encryptionRequested:0x7\r\na=rtpmap:98 AV1/90000\r\n");
    assert_eq!((d.hevc, d.av1, d.encryption_supported, d.encryption_requested), (false, true, 7, 7));
    assert!(d.offers(VideoCodec::H264) && d.offers(VideoCodec::Av1) && !d.offers(VideoCodec::H265));
    // A host that says nothing of this (GeForce Experience).
    assert_eq!(parse_describe("a=fmtp:97 surround-params=21101\n"), Describe::default());
}

#[test]
fn private_addresses_as_moonlight_counts_them() {
    let private = ["10.1.2.3", "172.16.0.1", "172.31.255.254", "192.168.1.20", "169.254.3.4", "fe80::1", "fd12:3456::1", "fec0::5"];
    let public = ["127.0.0.1", "172.32.0.1", "8.8.8.8", "100.64.0.1", "192.169.0.1", "2001:db8::1", "::1"];
    for a in private {
        assert!(is_private_network_address(&a.parse().unwrap()), "{a}");
    }
    for a in public {
        assert!(!is_private_network_address(&a.parse().unwrap()), "{a}");
    }
}

#[test]
fn ports_and_hosts_of_session_urls() {
    assert_eq!(rtsp_port_from_url(Some("rtspenc://192.168.1.20:48010")), 48010);
    assert_eq!(rtsp_port_from_url(Some("rtsp://[fe80::1]:51000")), 51000);
    assert_eq!(rtsp_port_from_url(Some("rtsp://host")), 48010);
    assert_eq!(rtsp_port_from_url(Some("rtsp://host:0")), 48010);
    assert_eq!(rtsp_port_from_url(None), 48010);
    assert_eq!(url_host("rtspenc://192.168.1.20:48010").as_deref(), Some("192.168.1.20"));
    assert_eq!(url_host("rtsp://[fe80::1]:51000").as_deref(), Some("[fe80::1]"));
    assert_eq!(url_host("rtsp://name/path").as_deref(), Some("name"));
    assert_eq!(url_host("nothing"), None);

    let key = [1u8; 16];
    let remote: IpAddr = "192.168.1.20".parse().unwrap();
    // High-quality audio: the host is named as the launch named it.
    let client = RtspClient::new(Some("rtspenc://192.168.1.20:48010"), remote, &key, AppVersion::SUNSHINE, true);
    assert_eq!((client.host_header.as_str(), client.target_url.as_str()), ("192.168.1.20", "rtspenc://192.168.1.20:48010"));
    assert!(client.is_encrypted());
    assert_eq!(client.address(), "192.168.1.20:48010".parse::<std::net::SocketAddr>().unwrap());
    // Otherwise 0.0.0.0, which a Sunshine host reads as "normal quality".
    let client = RtspClient::new(Some("rtsp://192.168.1.20:48011"), remote, &key, AppVersion::SUNSHINE, false);
    assert_eq!((client.host_header.as_str(), client.target_url.as_str()), ("0.0.0.0", "rtsp://0.0.0.0:48011"));
    assert!(!client.is_encrypted());
    // No session URL (old hosts): the address itself.
    let client = RtspClient::new(None, remote, &key, AppVersion::SUNSHINE, true);
    assert_eq!((client.host_header.as_str(), client.target_url.as_str()), ("192.168.1.20", "rtsp://192.168.1.20:48010"));
}

fn request(codec: VideoCodec) -> StreamRequest {
    StreamRequest { codec, width: 1920, height: 1080, fps: 60, bitrate_kbps: 20000, encryption_flags: ENCFLG_ALL }
}

/// Sunshine on a private network with default settings.
const LAN: Describe = Describe {
    hevc: true,
    av1: false,
    reference_frame_invalidation: true,
    feature_flags: 3,
    encryption_supported: SS_ENC_CONTROL_V2 | SS_ENC_AUDIO,
    encryption_requested: SS_ENC_CONTROL_V2,
};

#[test]
fn the_sdp_for_a_sunshine_host_on_the_same_network() {
    let remote: IpAddr = "192.168.1.20".parse().unwrap();
    let (sdp, negotiated) = build_sdp(&request(VideoCodec::H264), &LAN, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 47998);
    // Line by line what SdpGenerator.c writes for these settings (every
    // attribute line ends with a space, the last line with two).
    let expected = [
        "v=0",
        "o=android 0 14 IN IPv4 192.168.1.20",
        "s=NVIDIA Streaming Client",
        "a=x-ml-general.featureFlags:3 ",
        "a=x-ss-general.encryptionEnabled:5 ",
        "a=x-ss-video[0].chromaSamplingType:0 ",
        "a=x-ss-video[0].intraRefresh:0 ",
        "a=x-nv-video[0].clientViewportWd:1920 ",
        "a=x-nv-video[0].clientViewportHt:1080 ",
        "a=x-nv-video[0].maxFPS:60 ",
        "a=x-nv-video[0].packetSize:1024 ",
        "a=x-nv-video[0].rateControlMode:4 ",
        "a=x-nv-video[0].timeoutLengthMs:7000 ",
        "a=x-nv-video[0].framesWithInvalidRefThreshold:0 ",
        "a=x-nv-video[0].initialBitrateKbps:16000 ",
        "a=x-nv-video[0].initialPeakBitrateKbps:16000 ",
        "a=x-nv-vqos[0].bw.minimumBitrateKbps:16000 ",
        "a=x-nv-vqos[0].bw.maximumBitrateKbps:16000 ",
        "a=x-ml-video.configuredBitrateKbps:20000 ",
        "a=x-nv-vqos[0].fec.enable:1 ",
        "a=x-nv-vqos[0].videoQualityScoreUpdateTime:5000 ",
        "a=x-nv-vqos[0].qosTrafficType:5 ",
        "a=x-nv-aqos.qosTrafficType:4 ",
        "a=x-nv-general.featureFlags:167 ",
        "a=x-nv-general.useReliableUdp:13 ",
        "a=x-nv-vqos[0].fec.minRequiredFecPackets:2 ",
        "a=x-nv-vqos[0].bllFec.enable:0 ",
        "a=x-nv-vqos[0].drc.enable:0 ",
        "a=x-nv-general.enableRecoveryMode:0 ",
        "a=x-nv-video[0].videoEncoderSlicesPerFrame:1 ",
        "a=x-nv-clientSupportHevc:0 ",
        "a=x-nv-vqos[0].bitStreamFormat:0 ",
        "a=x-nv-video[0].dynamicRangeMode:0 ",
        "a=x-nv-video[0].maxNumReferenceFrames:1 ",
        "a=x-nv-video[0].clientRefreshRateX100:6000 ",
        "a=x-nv-audio.surround.numChannels:2 ",
        "a=x-nv-audio.surround.channelMask:3 ",
        "a=x-nv-audio.surround.enable:0 ",
        "a=x-nv-audio.surround.AudioQuality:0 ",
        "a=x-nv-aqos.packetDuration:5 ",
        "a=x-nv-video[0].encoderCscMode:2 ",
        "t=0 0",
        "m=video 47998  ",
        "",
    ]
    .join("\r\n");
    assert_eq!(sdp, expected);
    assert_eq!(
        negotiated,
        Negotiated {
            encryption_enabled: SS_ENC_CONTROL_V2 | SS_ENC_AUDIO,
            video_encrypted: false,
            audio_encrypted: true,
            packet_size: 1024,
            audio_packet_duration_ms: 5,
            remote: false,
        }
    );
}

fn attribute<'a>(sdp: &'a str, name: &str) -> Option<&'a str> {
    sdp.lines().find_map(|l| l.strip_prefix("a=")?.strip_prefix(name)?.strip_prefix(':')).map(|v| v.trim_end())
}

#[test]
fn the_sdp_follows_the_codec_the_network_and_the_encryption() {
    // H.265 and AV1.
    let remote: IpAddr = "192.168.1.20".parse().unwrap();
    let (sdp, _) = build_sdp(&request(VideoCodec::H265), &LAN, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 47998);
    assert_eq!(attribute(&sdp, "x-nv-clientSupportHevc"), Some("1"));
    assert_eq!(attribute(&sdp, "x-nv-vqos[0].bitStreamFormat"), Some("1"));
    assert_eq!(attribute(&sdp, "x-nv-video[0].encoderFeatureSetting"), None);
    let (sdp, _) = build_sdp(&request(VideoCodec::Av1), &LAN, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 47998);
    assert_eq!(attribute(&sdp, "x-nv-clientSupportHevc"), None);
    assert_eq!(attribute(&sdp, "x-nv-vqos[0].bitStreamFormat"), Some("2"));

    // Not a private address (STREAM_CFG_REMOTE): 1024-byte packets over IPv4,
    // 500 kbit/s kept back, no QoS marking; the host offers video encryption
    // there (wan_encryption_mode 1) and it is taken: 32 bytes less per packet.
    let wan = Describe { encryption_supported: 7, encryption_requested: 1, ..LAN };
    let remote: IpAddr = "8.8.4.4".parse().unwrap();
    let (sdp, negotiated) = build_sdp(&request(VideoCodec::H264), &wan, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 47998);
    assert_eq!(attribute(&sdp, "x-ss-general.encryptionEnabled"), Some("7"));
    assert_eq!(attribute(&sdp, "x-nv-video[0].packetSize"), Some("992"));
    assert_eq!(attribute(&sdp, "x-nv-vqos[0].bw.maximumBitrateKbps"), Some("15500"));
    assert_eq!(attribute(&sdp, "x-ml-video.configuredBitrateKbps"), Some("20000"));
    assert_eq!(attribute(&sdp, "x-nv-vqos[0].qosTrafficType"), Some("0"));
    assert_eq!(attribute(&sdp, "x-nv-aqos.qosTrafficType"), Some("0"));
    assert!(negotiated.video_encrypted && negotiated.audio_encrypted && negotiated.remote);
    assert_eq!(negotiated.packet_size, 992);

    // IPv6, not private: 1184-byte packets; the address in brackets.
    let remote: IpAddr = "2001:db8::7".parse().unwrap();
    let (sdp, negotiated) = build_sdp(&request(VideoCodec::H264), &LAN, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 50000);
    assert!(sdp.starts_with("v=0\r\no=android 0 14 IN IPv6 [2001:db8::7]\r\ns=NVIDIA Streaming Client\r\n"));
    assert!(sdp.ends_with("t=0 0\r\nm=video 50000  \r\n"));
    assert_eq!(negotiated.packet_size, 1184);

    // A client that asks for no encryption of its own still follows the host:
    // nothing when the host only offers, everything it demands.
    let mut plain = request(VideoCodec::H264);
    plain.encryption_flags = 0;
    let remote: IpAddr = "192.168.1.20".parse().unwrap();
    let (sdp, negotiated) = build_sdp(&plain, &wan, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 47998);
    assert_eq!(attribute(&sdp, "x-ss-general.encryptionEnabled"), Some("1"));
    assert_eq!(attribute(&sdp, "x-nv-general.featureFlags"), Some("135"));
    assert!(!negotiated.video_encrypted && !negotiated.audio_encrypted);
    let mandatory = Describe { encryption_supported: 7, encryption_requested: 7, ..LAN };
    let (sdp, negotiated) = build_sdp(&plain, &mandatory, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 47998);
    assert_eq!(attribute(&sdp, "x-ss-general.encryptionEnabled"), Some("7"));
    assert_eq!(attribute(&sdp, "x-nv-general.featureFlags"), Some("167"));
    assert_eq!(attribute(&sdp, "x-nv-video[0].packetSize"), Some("992"));
    assert!(negotiated.video_encrypted && negotiated.audio_encrypted);

    // A host without the Sunshine flags at all (an old one): the old control
    // encryption, audio still encrypted by the NVIDIA feature flag.
    let (sdp, negotiated) = build_sdp(&request(VideoCodec::H264), &Describe::default(), AppVersion::SUNSHINE, &remote, Reachability::Unknown, 47998);
    assert_eq!(attribute(&sdp, "x-ss-general.encryptionEnabled"), Some("0"));
    assert_eq!(attribute(&sdp, "x-nv-general.featureFlags"), Some("167"));
    assert!(negotiated.audio_encrypted && !negotiated.video_encrypted);

    // The bitrate cap of 100 Mbit/s and a small bitrate far away.
    let mut big = request(VideoCodec::H264);
    big.bitrate_kbps = 150_000;
    let (sdp, _) = build_sdp(&big, &LAN, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 47998);
    assert_eq!(attribute(&sdp, "x-nv-vqos[0].bw.maximumBitrateKbps"), Some("100000"));
    assert_eq!(attribute(&sdp, "x-ml-video.configuredBitrateKbps"), Some("150000"));
    let mut small = request(VideoCodec::H264);
    small.bitrate_kbps = 500;
    let (sdp, _) = build_sdp(&small, &LAN, AppVersion::SUNSHINE, &"8.8.8.8".parse().unwrap(), Reachability::Unknown, 47998);
    assert_eq!(attribute(&sdp, "x-nv-vqos[0].bw.maximumBitrateKbps"), Some("400"));
}

#[test]
fn video_packets_fit_any_path_unless_the_host_is_on_the_link() {
    // Found with the first real host: 192.168.1.20 was reached through a
    // WireGuard tunnel (MTU 1420). With Moonlight's rule for an unknown
    // route (a private address is "local": 1392-byte packets, 1436 bytes of
    // IP packet) only the short packets of small frames arrived and no key
    // frame ever did. Whatever the address, a video datagram must fit the
    // smallest path there is, when the route is a VPN or not known.
    for address in ["192.168.1.20", "10.0.0.2", "172.16.5.5", "169.254.1.1", "8.8.8.8", "127.0.0.1", "fe80::1", "fd00::2", "2001:db8::1"] {
        let remote: IpAddr = address.parse().unwrap();
        for supported in [SS_ENC_CONTROL_V2 | SS_ENC_AUDIO, 7] {
            let describe = Describe { encryption_supported: supported, ..LAN };
            for reach in [Reachability::Unknown, Reachability::Vpn] {
                let (sdp, negotiated) = build_sdp(&request(VideoCodec::H264), &describe, AppVersion::SUNSHINE, &remote, reach, 47998);
                assert_eq!(attribute(&sdp, "x-nv-video[0].packetSize"), Some(negotiated.packet_size.to_string().as_str()));
                // stream.cpp: blocksize = packetsize + MAX_RTP_HEADER_SIZE (16),
                // plus the 32 bytes of video_packet_enc_prefix_t when encrypted;
                // then UDP (8) and IPv4 (20) or IPv6 (40).
                let datagram = negotiated.packet_size + 16 + if negotiated.video_encrypted { 32 } else { 0 };
                let ip_packet = datagram + 8 + if remote.is_ipv4() { 20 } else { 40 };
                assert!(ip_packet <= 1280, "{address}: {ip_packet} bytes");
                assert_eq!(negotiated.packet_size % 16, 0);
                if reach == Reachability::Vpn {
                    // moonlight-qt session.cpp RI_VPN: remote "even if the
                    // target address is in RFC 1918 address space", 1024 bytes.
                    assert!(negotiated.remote);
                    assert_eq!(attribute(&sdp, "x-nv-vqos[0].qosTrafficType"), Some("0"));
                    assert_eq!(negotiated.packet_size, if negotiated.video_encrypted { 992 } else { 1024 });
                }
            }
            // On the link (moonlight-qt RI_LAN): local whatever the address,
            // 1392-byte packets, which fit an Ethernet frame encrypted or not.
            let (sdp, negotiated) = build_sdp(&request(VideoCodec::H264), &describe, AppVersion::SUNSHINE, &remote, Reachability::Lan, 47998);
            assert!(!negotiated.remote);
            assert_eq!(attribute(&sdp, "x-nv-vqos[0].qosTrafficType"), Some("5"));
            assert_eq!(negotiated.packet_size, if negotiated.video_encrypted { 1360 } else { 1392 });
            let datagram = negotiated.packet_size + 16 + if negotiated.video_encrypted { 32 } else { 0 };
            assert!(datagram + 8 + if remote.is_ipv4() { 20 } else { 40 } <= 1500, "{address}");
        }
    }
}

#[test]
fn the_largest_announce_fits_the_hosts_buffer() {
    // rtsp.cpp socket_t::msg_buf is 2048 bytes for the whole request; an
    // encrypted one must satisfy 24 + length < 2048.
    let request = StreamRequest {
        codec: VideoCodec::H265,
        width: 8192,
        height: 8192,
        fps: 240,
        bitrate_kbps: 150_000,
        encryption_flags: ENCFLG_ALL,
    };
    let describe = Describe { encryption_supported: 7, encryption_requested: 7, ..LAN };
    let remote: IpAddr = "2001:db8:1111:2222:3333:4444:5555:6666".parse().unwrap();
    let (sdp, _) = build_sdp(&request, &describe, AppVersion::SUNSHINE, &remote, Reachability::Unknown, 65535);
    let announce = serialize_request(
        "ANNOUNCE",
        "streamid=control/13/0",
        &[
            ("CSeq", "999".into()),
            ("X-GS-ClientVersion", "14".into()),
            ("Host", "[2001:db8:1111:2222:3333:4444:5555:6666]".into()),
            ("Session", "DEADBEEFCAFE".into()),
            ("Content-type", "application/sdp".into()),
            ("Content-length", sdp.len().to_string()),
        ],
        sdp.as_bytes(),
    );
    assert!(ENC_RTSP_HEADER_SIZE + announce.len() < HOST_REQUEST_BUFFER, "{} bytes", announce.len());
}

// ---------------------------------------------------------------------------
// The exchange against the host
// ---------------------------------------------------------------------------

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// A paired client, a launch (`corever` as given), and the RTSP client for it.
async fn launched(host: &FakeHost, core_version: u32, high_quality_audio: bool) -> (HostClient, LaunchParams, RtspClient) {
    let (identity, server_cert) = host.pair_directly("00000000000000C1");
    let client = HostClient::new("127.0.0.1", host.http_port, &identity, Some(&server_cert)).unwrap();
    let info = client.server_info().await.unwrap();
    let mut params = LaunchParams::new(DESKTOP_ID, 1280, 720, 60).unwrap();
    params.core_version = core_version;
    let answer = client.start_app(&info, &params).await.unwrap();
    let rtsp = RtspClient::new(answer.session_url.as_deref(), LOCALHOST, &params.rikey, AppVersion::SUNSHINE, high_quality_audio);
    (client, params, rtsp)
}

async fn whole_exchange(encrypted: bool) {
    let host = FakeHost::start().await;
    let (_client, params, mut rtsp) = launched(&host, if encrypted { 1 } else { 0 }, true).await;
    assert_eq!(rtsp.is_encrypted(), encrypted);

    rtsp.options().await.unwrap();
    let describe = rtsp.describe().await.unwrap();
    assert_eq!(
        describe,
        Describe {
            hevc: true,
            av1: false,
            reference_frame_invalidation: true,
            feature_flags: 3,
            encryption_supported: SS_ENC_CONTROL_V2 | SS_ENC_AUDIO,
            encryption_requested: SS_ENC_CONTROL_V2,
        }
    );
    let (ports, launch) = {
        let st = host.state.lock().unwrap();
        ((st.stream.audio_port, st.stream.video_port, st.stream.control_port), st.stream.launch.clone().unwrap())
    };
    let ping: [u8; 16] = launch.av_ping_payload.as_bytes().try_into().unwrap();
    assert_eq!(rtsp.setup_audio().await.unwrap(), (ports.0, Some(ping)));
    assert_eq!(rtsp.setup_video().await.unwrap(), (ports.1, Some(ping)));
    assert_eq!(rtsp.setup_control().await.unwrap(), (ports.2, launch.control_connect_data));

    let request = StreamRequest {
        codec: VideoCodec::H265,
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_kbps: 20000,
        encryption_flags: ENCFLG_ALL,
    };
    let (sdp, negotiated) = build_sdp(&request, &describe, AppVersion::SUNSHINE, &LOCALHOST, Reachability::Unknown, ports.1);
    rtsp.announce(&sdp).await.unwrap();
    rtsp.play().await.unwrap();

    let st = host.state.lock().unwrap();
    let seen = &st.stream.rtsp;
    // The order of RtspConnection.c performRtspHandshake.
    let url = format!("{}://127.0.0.1:{}", if encrypted { "rtspenc" } else { "rtsp" }, host.rtsp_port);
    let expected: Vec<(&str, &str)> = vec![
        ("OPTIONS", url.as_str()),
        ("DESCRIBE", url.as_str()),
        ("SETUP", "streamid=audio/0/0"),
        ("SETUP", "streamid=video/0/0"),
        ("SETUP", "streamid=control/13/0"),
        ("ANNOUNCE", "streamid=control/13/0"),
        ("PLAY", "/"),
    ];
    assert_eq!(seen.iter().map(|r| (r.method.as_str(), r.target.as_str())).collect::<Vec<_>>(), expected);
    for (index, r) in seen.iter().enumerate() {
        assert_eq!(r.encrypted, encrypted);
        // CSeq counts from 1; the encryption's own number too, one per message.
        assert_eq!(r.header("CSeq"), Some((index + 1).to_string().as_str()));
        assert_eq!(r.seq, if encrypted { index as u32 + 1 } else { 0 });
        assert_eq!(r.header("X-GS-ClientVersion"), Some("14"));
        assert_eq!(r.header("Host"), Some("127.0.0.1"));
        assert!(r.wire_len < HOST_REQUEST_BUFFER);
    }
    assert_eq!(seen[1].header("Accept"), Some("application/sdp"));
    assert_eq!(seen[1].header("If-Modified-Since"), Some("Thu, 01 Jan 1970 00:00:00 GMT"));
    assert_eq!(seen[2].header("Session"), None);
    for setup in &seen[2..5] {
        assert_eq!(setup.header("Transport"), Some("unicast;X-GS-ClientPort=50000-50001"));
        assert_eq!(setup.header("If-Modified-Since"), Some("Thu, 01 Jan 1970 00:00:00 GMT"));
    }
    // The session id without what follows the ';'.
    for with_session in &seen[3..7] {
        assert_eq!(with_session.header("Session"), Some("DEADBEEFCAFE"));
    }
    assert_eq!(seen[5].header("Content-type"), Some("application/sdp"));
    assert_eq!(seen[5].header("Content-length"), Some(sdp.len().to_string().as_str()));
    assert_eq!(seen[5].payload, sdp);

    // What rtsp.cpp cmd_announce made of the SDP, field by field.
    let session = st.stream.session.as_ref().expect("the ANNOUNCE started a session");
    let config = &session.config;
    assert_eq!(config.client, "NVIDIA Streaming Client");
    assert_eq!((config.width, config.height, config.framerate, config.framerate_x100), (1280, 720, 60, 6000));
    // 127.0.0.1 is not a private address for Moonlight: the remote settings.
    assert_eq!(config.packetsize, 1024);
    assert_eq!(config.packetsize, negotiated.packet_size);
    assert_eq!(config.bitrate, 15500);
    assert_eq!(config.configured_bitrate_kbps, 20000);
    assert_eq!((config.video_qos_type, config.audio_qos_type), (0, 0));
    assert_eq!(config.min_required_fec_packets, 2);
    assert_eq!(config.ml_feature_flags, ML_FF_FEC_STATUS | ML_FF_SESSION_ID_V1);
    assert_eq!(config.control_protocol_type, 13);
    assert_eq!(config.encryption_flags_enabled, SS_ENC_CONTROL_V2 | SS_ENC_AUDIO);
    assert_eq!(config.video_format, 1); // H.265
    assert_eq!((config.dynamic_range, config.chroma_sampling_type, config.enable_intra_refresh), (0, 0, 0));
    assert_eq!((config.slices_per_frame, config.num_ref_frames, config.encoder_csc_mode), (1, 1, 2));
    assert_eq!((config.audio_channels, config.audio_mask, config.audio_packet_duration), (2, 3, 5));
    // The Host option named the host: high-quality stereo.
    assert!(config.audio_high_quality);
    assert_eq!(session.launch.key, params.rikey);
    assert_eq!(session.launch.rikeyid, params.rikey_id as u32);
    assert!(negotiated.audio_encrypted && !negotiated.video_encrypted && negotiated.remote);
}

#[tokio::test]
async fn the_exchange_in_plain_text() {
    whole_exchange(false).await;
}

#[tokio::test]
async fn the_exchange_encrypted() {
    whole_exchange(true).await;
}

#[tokio::test]
async fn low_bitrate_asks_for_normal_audio_through_the_host_option() {
    let host = FakeHost::start().await;
    let (_client, _params, mut rtsp) = launched(&host, 1, false).await;
    rtsp.options().await.unwrap();
    let describe = rtsp.describe().await.unwrap();
    rtsp.setup_audio().await.unwrap();
    let (video_port, _) = rtsp.setup_video().await.unwrap();
    rtsp.setup_control().await.unwrap();
    let request = StreamRequest { codec: VideoCodec::H264, width: 1280, height: 720, fps: 30, bitrate_kbps: 5000, encryption_flags: ENCFLG_ALL };
    let (sdp, _) = build_sdp(&request, &describe, AppVersion::SUNSHINE, &LOCALHOST, Reachability::Unknown, video_port);
    rtsp.announce(&sdp).await.unwrap();
    let st = host.state.lock().unwrap();
    assert_eq!(st.stream.rtsp[0].target, format!("rtsp://0.0.0.0:{}", host.rtsp_port));
    assert!(st.stream.rtsp.iter().all(|r| r.header("Host") == Some("0.0.0.0")));
    let config = &st.stream.session.as_ref().unwrap().config;
    assert!(!config.audio_high_quality);
    assert_eq!((config.video_format, config.framerate, config.bitrate), (0, 30, 3500));
}

#[tokio::test]
async fn what_the_host_refuses() {
    // A codec the host has switched off: 400.
    let host = FakeHost::start().await;
    host.state.lock().unwrap().stream.hevc = false;
    let (_client, _params, mut rtsp) = launched(&host, 1, true).await;
    rtsp.options().await.unwrap();
    let describe = rtsp.describe().await.unwrap();
    assert!(!describe.hevc);
    rtsp.setup_audio().await.unwrap();
    let (video_port, _) = rtsp.setup_video().await.unwrap();
    rtsp.setup_control().await.unwrap();
    let (sdp, _) = build_sdp(&request(VideoCodec::H265), &describe, AppVersion::SUNSHINE, &LOCALHOST, Reachability::Unknown, video_port);
    assert!(matches!(rtsp.announce(&sdp).await, Err(Error::Host { code: 400, .. })));
    assert!(host.state.lock().unwrap().stream.session.is_none());

    // Mandatory encryption and a client that was told to enable none: 403,
    // worded for the person. (build_sdp never does this by itself: it follows
    // what the host requests; the refusal is forced with an edited SDP.)
    let host = FakeHost::start().await;
    host.state.lock().unwrap().stream.encryption_mode = EncryptionMode::Mandatory;
    let (_client, _params, mut rtsp) = launched(&host, 1, true).await;
    rtsp.options().await.unwrap();
    let describe = rtsp.describe().await.unwrap();
    assert_eq!((describe.encryption_supported, describe.encryption_requested), (7, 7));
    rtsp.setup_audio().await.unwrap();
    let (video_port, _) = rtsp.setup_video().await.unwrap();
    rtsp.setup_control().await.unwrap();
    let (sdp, negotiated) = build_sdp(&request(VideoCodec::H264), &describe, AppVersion::SUNSHINE, &LOCALHOST, Reachability::Unknown, video_port);
    assert_eq!(negotiated.encryption_enabled, 7);
    let weakened = sdp.replace("a=x-ss-general.encryptionEnabled:7 ", "a=x-ss-general.encryptionEnabled:1 ").replace(
        "a=x-nv-general.featureFlags:167 ",
        "a=x-nv-general.featureFlags:135 ",
    );
    assert_eq!(rtsp.announce(&weakened).await, Err(Error::EncryptionRequired));
    // As asked for, it is taken.
    rtsp.announce(&sdp).await.unwrap();
    assert_eq!(host.state.lock().unwrap().stream.session.as_ref().unwrap().config.encryption_flags_enabled, 7);

    // No launch waits for RTSP: the host closes the connection without a word.
    let host = FakeHost::start().await;
    let mut rtsp = RtspClient::new(
        Some(&format!("rtspenc://127.0.0.1:{}", host.rtsp_port)),
        LOCALHOST,
        &[3; 16],
        AppVersion::SUNSHINE,
        true,
    );
    assert_eq!(rtsp.options().await, Err(Error::ConnectionLost));

    // A key that is not the launch's: the host cannot read the request and
    // its "400" comes back sealed with the right key, which this side cannot open.
    let host = FakeHost::start().await;
    let (_client, _params, _rtsp) = launched(&host, 1, true).await;
    let mut wrong = RtspClient::new(
        Some(&format!("rtspenc://127.0.0.1:{}", host.rtsp_port)),
        LOCALHOST,
        &[3; 16],
        AppVersion::SUNSHINE,
        true,
    );
    assert!(matches!(wrong.options().await, Err(Error::Protocol(_))));
    assert!(host.state.lock().unwrap().stream.rtsp.is_empty());
}
