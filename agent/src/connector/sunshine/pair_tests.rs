//! Tests of pairing and of the HTTPS requests against the host that runs
//! inside the test (`fake_host.rs`, written from Sunshine src/nvhttp.cpp).

use std::time::Duration;

use tokio::net::TcpListener;

use super::super::fake_host::{host_keys, FakeHost, DESKTOP_ID};
use super::super::http::{
    cert_der_from_pem, cert_parts, hex_decode, hex_encode, rsa_sign_sha256, Error, HostClient,
    Identity, LaunchParams,
};
use super::*;

// ---------------------------------------------------------------------------
// Vectors
// ---------------------------------------------------------------------------

#[test]
fn aes_key_from_salt_and_pin() {
    // Sunshine crypto.cpp gen_aes_key: SHA-256(salt + pin), first 16 bytes.
    // Expected values from Python's hashlib for salt 10 11 .. 1f and PIN "1234".
    let salt: [u8; 16] = std::array::from_fn(|i| 0x10 + i as u8);
    assert_eq!(hex_encode(&derive_aes_key(&salt, "1234", true)), "2707dcc9278c8c1f3bd7e2ef4f57b7e5");
    // Hosts before generation 7: SHA-1 (libgamestream/client.c gs_pair).
    assert_eq!(hex_encode(&derive_aes_key(&salt, "1234", false)), "3ce43f4044b4ded42291c294ea35cc9b");
    assert_ne!(derive_aes_key(&salt, "1235", true), derive_aes_key(&salt, "1234", true));
    assert_eq!(pairing_hash(true, b"").len(), 32);
    assert_eq!(pairing_hash(false, b"").len(), 20);
}

#[test]
fn aes_ecb_blocks() {
    // FIPS-197 appendix C.1.
    let key: [u8; 16] = std::array::from_fn(|i| i as u8);
    let plain = hex_decode("00112233445566778899aabbccddeeff").unwrap();
    let cipher = aes_ecb_encrypt(&key, &plain);
    assert_eq!(hex_encode(&cipher), "69c4e0d86a7b0430d8cdb78070b4c55a");
    assert_eq!(aes_ecb_decrypt(&key, &cipher), plain);

    // Two blocks under the key derived above, expected from
    // `openssl enc -aes-128-ecb -nopad`: each block alone, no chaining, no padding.
    let key: [u8; 16] = hex_decode("2707dcc9278c8c1f3bd7e2ef4f57b7e5").unwrap().try_into().unwrap();
    let plain: Vec<u8> = (0..32u8).collect();
    let cipher = aes_ecb_encrypt(&key, &plain);
    assert_eq!(
        hex_encode(&cipher),
        "8ba7d4059b6a074173d9bb082959d3f12460b22291b9fe7f3a1456d902db0346"
    );
    assert_eq!(aes_ecb_decrypt(&key, &cipher), plain);
    assert_eq!(aes_ecb_encrypt(&key, &plain[..16]), cipher[..16]);
    // Only whole blocks (EVP_EncryptUpdate without the final call).
    assert_eq!(aes_ecb_encrypt(&key, &plain[..20]).len(), 16);
    assert_eq!(aes_ecb_decrypt(&key, &cipher[..47 - 16]).len(), 16);
}

#[test]
fn pins_are_four_digits() {
    for _ in 0..200 {
        let pin = generate_pin().unwrap();
        assert_eq!(pin.len(), 4);
        assert!(pin.bytes().all(|b| b.is_ascii_digit()));
    }
}

fn client_identity() -> &'static Identity {
    super::super::http::tests::test_identity()
}

/// The shared identity with another unique id: Sunshine keys pairing sessions
/// by it, and tests run side by side against separate hosts anyway.
fn identity_with_id(id: &str) -> Identity {
    let mut identity = client_identity().clone();
    identity.unique_id = id.to_string();
    identity
}

const LONG: Duration = Duration::from_secs(60);

async fn pair_with(host: &FakeHost, identity: &Identity, shown: &str, typed: &str) -> Result<Paired, Error> {
    let port = host.http_port;
    let (identity, shown) = (identity.clone(), shown.to_string());
    let task = tokio::spawn(async move { pair("127.0.0.1", port, &identity, &shown, "Lunaris test & more", LONG).await });
    assert_eq!(host.wait_pending().await, "Lunaris test & more");
    host.enter_pin(typed);
    task.await.unwrap()
}

// ---------------------------------------------------------------------------
// Pairing and what follows it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pairs_and_talks_https() {
    let host = FakeHost::start().await;
    let identity = identity_with_id("00000000000000A1");

    // Before pairing: HTTP only, not paired, nothing over HTTPS.
    let before = HostClient::new("127.0.0.1", host.http_port, &identity, None).unwrap();
    let info = before.server_info().await.unwrap();
    assert!(!info.paired && !info.over_https);
    assert_eq!(info.hostname, "fake-host");
    assert_eq!(before.app_list().await, Err(Error::NotPaired));

    let paired = pair_with(&host, &identity, "4417", "4417").await.unwrap();
    assert_eq!(paired.server_cert_pem, host_keys().cert_pem);
    assert_eq!(host.paired_count(), 1);
    assert_eq!(host.session_count(), 0);
    // The last step went over HTTPS; the reference asks no /unpair after a success.
    let requests = host.state.lock().unwrap().requests.clone();
    assert_eq!(requests.last().map(String::as_str), Some("https /pair"));
    assert!(!requests.iter().any(|r| r.contains("unpair")));

    let client = HostClient::new("127.0.0.1", host.http_port, &identity, Some(&paired.server_cert_pem)).unwrap();
    let info = client.server_info().await.unwrap();
    assert!(info.paired && info.over_https);
    assert_eq!(client.https_port(), host.https_port);
    assert_eq!(info.mac, "02:00:00:00:00:01");
    assert_eq!(info.current_game, 0);

    let apps = client.app_list().await.unwrap();
    assert_eq!(apps[0].title, "Desktop");
    assert_eq!(apps[0].id, DESKTOP_ID);
    assert_eq!(apps[1].title, "Steam & Big Picture");

    // launch: what the host reads (nvhttp.cpp launch, make_launch_session) arrives as sent.
    let params = LaunchParams::new(DESKTOP_ID, 1280, 720, 60).unwrap();
    let answer = client.start_app(&info, &params).await.unwrap();
    assert!(!answer.resumed);
    assert_eq!(answer.session_url, Some(format!("rtspenc://127.0.0.1:{}", host.rtsp_port)));
    {
        let st = host.state.lock().unwrap();
        let args = st.launch_args.as_ref().unwrap();
        assert_eq!(args["uniqueid"], "00000000000000A1");
        assert_eq!(args["rikey"], hex_encode(&params.rikey));
        // make_launch_session: big-endian of rikeyid is the IV's start.
        let id: i32 = args["rikeyid"].parse().unwrap();
        assert_eq!(id.to_be_bytes(), params.rikey_iv()[..4]);
        assert_eq!(args["mode"], "1280x720x60");
        assert_eq!(args["appid"], DESKTOP_ID.to_string());
        assert_eq!(args["localAudioPlayMode"], "0");
        assert_eq!(args["surroundAudioInfo"], "196610");
        assert_eq!(args["corever"], "1");
        assert_eq!(args["sops"], "0");
        assert_eq!(args["gcmap"], "0");
    }

    // A second launch is refused; with the fresh serverinfo it becomes a resume.
    assert_eq!(client.launch(&params).await, Err(Error::Busy));
    let info = client.server_info().await.unwrap();
    assert!(info.is_busy());
    assert_eq!(info.current_game as i32, DESKTOP_ID);
    let mut plain = LaunchParams::new(DESKTOP_ID, 1280, 720, 60).unwrap();
    plain.core_version = 0;
    let answer = client.start_app(&info, &plain).await.unwrap();
    assert!(answer.resumed);
    assert_eq!(answer.session_url, Some(format!("rtsp://127.0.0.1:{}", host.rtsp_port)));

    client.cancel().await.unwrap();
    assert_eq!(client.server_info().await.unwrap().current_game, 0);
    assert!(matches!(client.resume(&params).await, Err(Error::Host { code: 503, .. })));
    let mut missing = params.clone();
    missing.app_id = 5;
    assert_eq!(client.launch(&missing).await, Err(Error::AppNotFound));

    // Another certificate than the pinned one: no request goes through.
    let other = super::super::http::tests::OPENSSL_CERT;
    let wrong = HostClient::new("127.0.0.1", host.http_port, &identity, Some(other)).unwrap();
    assert_eq!(wrong.app_list().await, Err(Error::HostChanged));
    // serverinfo says so too: what anybody answers over plain HTTP is not
    // taken in place of the computer that was paired ("pair again" would be
    // the wrong advice when something else answers at its address).
    assert_eq!(wrong.server_info().await, Err(Error::HostChanged));
    assert!(Error::HostChanged.to_string().contains("is not the one that was paired"));
    // A host that knows the certificate it shows but not this client: that
    // is "not paired", and only then is plain HTTP asked.
    let unknown = Identity::generate().unwrap();
    let stranger = HostClient::new("127.0.0.1", host.http_port, &unknown, Some(&paired.server_cert_pem)).unwrap();
    let info = stranger.server_info().await.unwrap();
    assert!(!info.paired && !info.over_https);
}

#[tokio::test]
async fn wrong_pin_is_told_and_leaves_nothing() {
    let host = FakeHost::start().await;
    let identity = identity_with_id("00000000000000A2");
    let result = pair_with(&host, &identity, "4417", "4471").await;
    assert_eq!(result.err(), Some(Error::PinRejected));
    assert_eq!(Error::PinRejected.to_string(), "The PIN was not accepted");
    // The host holds no session (a new attempt is not answered 409) and no certificate.
    assert_eq!(host.session_count(), 0);
    assert_eq!(host.paired_count(), 0);
    // As the reference: /unpair after a failure.
    assert!(host.state.lock().unwrap().requests.iter().any(|r| r == "http /unpair"));

    // The host does not know the certificate: HTTPS answers 401.
    let client = HostClient::new("127.0.0.1", host.http_port, &identity, Some(&host_keys().cert_pem)).unwrap();
    assert_eq!(client.app_list().await, Err(Error::NotPaired));
    let info = client.server_info().await.unwrap();
    assert!(!info.paired && !info.over_https);

    // A second attempt with the right PIN works at once.
    pair_with(&host, &identity, "0042", "0042").await.unwrap();
    assert_eq!(host.paired_count(), 1);
    assert!(client.server_info().await.unwrap().paired);
}

#[tokio::test]
async fn host_without_the_pin_check_on_our_side_would_refuse_too() {
    // The host's own check (nvhttp.cpp clientpairingsecret: same_hash) against
    // a client that skips the PIN comparison: run the stages by hand with a
    // wrong key and see paired = 0.
    let host = FakeHost::start().await;
    let identity = identity_with_id("00000000000000A7");
    let client = HostClient::new("127.0.0.1", host.http_port, &identity, None).unwrap();
    let salt = [7u8; 16];
    let cert_hex = hex_encode(identity.client_cert_pem.as_bytes());
    let first = {
        let query = format!("devicename=x&updateState=1&phrase=getservercert&salt={}&clientcert={cert_hex}", hex_encode(&salt));
        let port = host.http_port;
        let identity = identity.clone();
        tokio::spawn(async move {
            HostClient::new("127.0.0.1", port, &identity, None).unwrap().get_http("pair", &query, LONG).await
        })
    };
    host.wait_pending().await;
    host.enter_pin("1111");
    first.await.unwrap().unwrap();
    let key = derive_aes_key(&salt, "2222", true);
    let answer = client
        .get_http("pair", &format!("clientchallenge={}", hex_encode(&aes_ecb_encrypt(&key, &[1u8; 16]))), LONG)
        .await
        .unwrap();
    let root = super::super::http::parse_xml(&answer).unwrap();
    let response = aes_ecb_decrypt(&key, &hex_decode(root.text_of("challengeresponse")).unwrap());
    assert_eq!(response.len(), 48);
    let signature = cert_parts(&cert_der_from_pem(&identity.client_cert_pem).unwrap()).unwrap().signature;
    let secret = [9u8; 16];
    let hash = pairing_hash(true, &[&response[32..48], &signature[..], &secret[..]].concat());
    client
        .get_http("pair", &format!("serverchallengeresp={}", hex_encode(&aes_ecb_encrypt(&key, &hash))), LONG)
        .await
        .unwrap();
    let mut pairing_secret = secret.to_vec();
    pairing_secret.extend_from_slice(&rsa_sign_sha256(&identity.client_key_pem, &secret).unwrap());
    let answer = client
        .get_http("pair", &format!("clientpairingsecret={}", hex_encode(&pairing_secret)), LONG)
        .await
        .unwrap();
    assert_eq!(super::super::http::parse_xml(&answer).unwrap().text_of("paired"), "0");
    assert_eq!(host.paired_count(), 0);
}

#[tokio::test]
async fn dropping_the_wait_closes_the_connection() {
    let host = FakeHost::start().await;
    let identity = identity_with_id("00000000000000A3");
    let port = host.http_port;
    let task = tokio::spawn(async move { pair("127.0.0.1", port, &identity, "1234", "dropper", LONG).await });
    host.wait_pending().await;
    assert!(!host.state.lock().unwrap().waiting_connection_closed);
    task.abort();
    let _ = task.await;
    // The host sees the held connection close, and is told to forget the session.
    for _ in 0..500 {
        if host.state.lock().unwrap().waiting_connection_closed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(host.state.lock().unwrap().waiting_connection_closed);
    host.wait_no_session().await;
    assert_eq!(host.paired_count(), 0);
}

#[tokio::test]
async fn waiting_for_the_pin_times_out() {
    let host = FakeHost::start().await;
    let identity = identity_with_id("00000000000000A4");
    let started = std::time::Instant::now();
    let result = pair("127.0.0.1", host.http_port, &identity, "1234", "slow", Duration::from_millis(300)).await;
    assert_eq!(result.err(), Some(Error::PairingTimeout));
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(Error::PairingTimeout.to_string(), "Pairing was not completed in time");
    host.wait_no_session().await;
}

#[tokio::test]
async fn refused_at_the_host() {
    let host = FakeHost::start().await;
    let identity = identity_with_id("00000000000000A5");
    let port = host.http_port;
    let id = identity.clone();
    let task = tokio::spawn(async move { pair("127.0.0.1", port, &id, "1234", "refused", LONG).await });
    host.wait_pending().await;
    host.refuse();
    assert_eq!(task.await.unwrap().err(), Some(Error::PairingRefused));
}

#[tokio::test]
async fn a_pairing_under_way_is_not_disturbed() {
    let host = FakeHost::start().await;
    let identity = identity_with_id("00000000000000A6");
    let port = host.http_port;
    let id = identity.clone();
    let first = tokio::spawn(async move { pair("127.0.0.1", port, &id, "1234", "first", LONG).await });
    host.wait_pending().await;
    // The same unique id again: 409 (nvhttp.cpp start_pairing_session ALREADY_EXISTS).
    let second = pair("127.0.0.1", host.http_port, &identity, "5678", "second", LONG).await;
    assert_eq!(second.err(), Some(Error::PairingInProgress));
    // The first one still waits and completes.
    assert_eq!(host.session_count(), 1);
    host.enter_pin("1234");
    first.await.unwrap().unwrap();
    assert_eq!(host.paired_count(), 1);
}

#[tokio::test]
async fn a_forged_host_secret_fails_the_pairing() {
    let host = FakeHost::start().await;
    host.state.lock().unwrap().bad_signature = true;
    let identity = identity_with_id("00000000000000A8");
    let result = pair_with(&host, &identity, "1234", "1234").await;
    assert_eq!(result.err(), Some(Error::PairingFailed));
    assert_eq!(host.session_count(), 0);
    assert_eq!(host.paired_count(), 0);
}

#[tokio::test]
async fn unreachable_host_and_bad_input() {
    // A port where nothing listens.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let identity = identity_with_id("00000000000000A9");
    let result = pair("127.0.0.1", port, &identity, "1234", "nobody", LONG).await;
    let error = result.err().unwrap();
    assert_eq!(error, Error::Unreachable);
    // The text for the person names no address.
    assert_eq!(error.to_string(), "Could not reach the computer");
    assert!(!error.to_string().contains("127.0.0.1"));

    assert!(matches!(pair("127.0.0.1", port, &identity, "12a4", "x", LONG).await, Err(Error::Internal(_))));
    assert!(matches!(pair("127.0.0.1", port, &identity, "12345", "x", LONG).await, Err(Error::Internal(_))));
    let mut broken = identity.clone();
    broken.client_key_pem = "nothing".into();
    assert!(matches!(pair("127.0.0.1", port, &broken, "1234", "x", LONG).await, Err(Error::Identity(_))));
}

#[tokio::test]
async fn a_host_that_says_nothing_times_out() {
    // Accepts and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hold = tokio::spawn(async move {
        let mut kept = Vec::new();
        loop {
            if let Ok((s, _)) = listener.accept().await {
                kept.push(s);
            }
        }
    });
    let target = "/serverinfo?uniqueid=0&uuid=0";
    let result = http_get("127.0.0.1", port, None, target, "serverinfo", Duration::from_millis(200)).await;
    assert_eq!(result.err(), Some(Error::Timeout));
    hold.abort();
}

/// The same pieces against OpenSSL itself, which is what a real host uses
/// (Sunshine links OpenSSL): needs the `openssl` program, touches no host.
/// Run with `cargo test -- --ignored openssl`.
#[tokio::test]
#[ignore]
async fn openssl_accepts_the_identity() {
    use std::process::{Command, Stdio};
    let identity = identity_with_id("00000000000000AA");
    let dir = std::env::temp_dir().join(format!("sunshine-p-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
    std::fs::write(path("client.pem"), &identity.client_cert_pem).unwrap();
    std::fs::write(path("host.pem"), &host_keys().cert_pem).unwrap();
    std::fs::write(path("host.key"), &host_keys().key_pem).unwrap();

    // Sunshine crypto.cpp cert_chain_t::verify: X509_verify_cert with the
    // certificate as its own trust anchor and X509_V_FLAG_PARTIAL_CHAIN.
    let out = Command::new("openssl")
        .args(["verify", "-partial_chain", "-CAfile", &path("client.pem"), &path("client.pem")])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // The subject and the algorithm of libgamestream/mkcert.c.
    let text = Command::new("openssl").args(["x509", "-noout", "-text", "-in", &path("client.pem")]).output().unwrap();
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("Version: 3"), "{text}");
    assert!(text.contains("sha256WithRSAEncryption"));
    assert!(text.contains("Public-Key: (2048 bit)"));
    assert!(text.contains("Subject: CN=NVIDIA GameStream Client") || text.contains("Subject: CN = NVIDIA GameStream Client"));
    // The key file is what OpenSSL reads as a private key that fits the certificate.
    std::fs::write(path("client.key"), &identity.client_key_pem).unwrap();
    let key = Command::new("openssl").args(["rsa", "-noout", "-check", "-in", &path("client.key")]).output().unwrap();
    assert!(key.status.success());

    // A TLS server of OpenSSL that demands a client certificate, as the host's does.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Command::new("openssl")
        .args(["s_server", "-accept", &format!("127.0.0.1:{port}"), "-cert", &path("host.pem"), "-key", &path("host.key")])
        .args(["-Verify", "1", "-www", "-quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let client = HostClient::new("127.0.0.1", port, &identity, Some(&host_keys().cert_pem)).unwrap();
    client.set_https_port(port);
    let mut answer = Err(Error::Unreachable);
    for _ in 0..50 {
        answer = client.get_https("serverinfo", "", Duration::from_secs(5)).await;
        if answer != Err(Error::Unreachable) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // And with another certificate pinned, the same server is turned down.
    let other = HostClient::new("127.0.0.1", port, &identity, Some(super::super::http::tests::OPENSSL_CERT)).unwrap();
    other.set_https_port(port);
    let refused = other.get_https("serverinfo", "", Duration::from_secs(5)).await;
    let _ = server.kill();
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(&dir);
    // `-www` answers a page that describes the connection.
    let page = answer.expect("a TLS exchange with OpenSSL");
    assert!(page.contains("Ciphers"), "{page}");
    assert_eq!(refused.err(), Some(Error::HostChanged));
}
