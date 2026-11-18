//! Pairing with a GameStream host: the exchange that makes the host accept
//! this client's certificate, checked by a PIN that the person types at the
//! host.
//!
//! Ported from moonlight-embedded libgamestream/client.c gs_pair; the two
//! checks that file lacks (the wrong PIN, a pairing already under way) are
//! those of moonlight-qt app/backend/nvpairingmanager.cpp
//! NvPairingManager::pair. The host's side was read in Sunshine src/nvhttp.cpp
//! (pair, getservercert, clientchallenge, serverchallengeresp,
//! clientpairingsecret) and src/crypto.cpp.
//!
//! Nothing here logs the PIN, the salt, a key or a certificate.

use std::time::Duration;

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

use super::http::{
    cert_der_from_pem, cert_parts, check_status, hex_decode, hex_encode, http_get, parse_xml,
    random_bytes, rsa_sign_sha256, rsa_verify_sha256, url_encode, xml_status, Error, HostClient,
    Identity, XmlNode, REQUEST_TIMEOUT,
};

/// How long [`pair`] waits for the PIN when the caller has no better number:
/// Sunshine itself gives a pairing up after five minutes (nvhttp.h
/// PAIRING_SESSION_TIMEOUT).
pub const DEFAULT_PIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// What a finished pairing gives: keep it with the [`Identity`].
#[derive(Clone, PartialEq, Eq)]
pub struct Paired {
    /// The host's certificate, PEM: the only one HTTPS to this host accepts
    /// from now on ([`HostClient::new`]).
    pub server_cert_pem: String,
}

impl std::fmt::Debug for Paired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Paired").finish_non_exhaustive()
    }
}

/// A PIN of four digits, as Moonlight makes one and as Sunshine demands
/// (nvhttp.cpp is_valid_pairing_pin: exactly four digits).
pub fn generate_pin() -> Result<String, Error> {
    loop {
        let n = u16::from_le_bytes(random_bytes::<2>()?);
        // Without the top of the range every PIN is equally likely.
        if n < 60_000 {
            return Ok(format!("{:04}", n % 10_000));
        }
    }
}

/// The hash of the pairing: SHA-256 for hosts of generation 7 and later,
/// SHA-1 before (libgamestream/client.c gs_pair `serverMajorVersion >= 7`).
pub(super) fn pairing_hash(sha256: bool, data: &[u8]) -> Vec<u8> {
    let algorithm = if sha256 {
        &ring::digest::SHA256
    } else {
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY
    };
    ring::digest::digest(algorithm, data).as_ref().to_vec()
}

/// The AES-128 key of a pairing: the first 16 bytes of hash(salt + PIN)
/// (libgamestream/client.c gs_pair salt_pin / aes_key; Sunshine crypto.cpp
/// gen_aes_key).
pub(super) fn derive_aes_key(salt: &[u8; 16], pin: &str, sha256: bool) -> [u8; 16] {
    let mut salt_pin = salt.to_vec();
    salt_pin.extend_from_slice(pin.as_bytes());
    let hash = pairing_hash(sha256, &salt_pin);
    let mut key = [0u8; 16];
    key.copy_from_slice(&hash[..16]);
    key
}

/// AES-128 in ECB mode without padding (libgamestream/client.c encrypt:
/// EVP_aes_128_ecb, padding off, one EncryptUpdate). Only whole blocks come
/// out: bytes after the last whole block are dropped, as there.
pub(super) fn aes_ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes128::new(&(*key).into());
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let mut block = block.into();
        cipher.encrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}

/// The reverse (libgamestream/client.c decrypt).
pub(super) fn aes_ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes128::new(&(*key).into());
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let mut block = block.into();
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}

/// What is left to tidy at the host when a pairing ends without success. It
/// runs when [`pair`] fails and also when its future is dropped (the person
/// closed the dialog): then as a task of its own on the runtime.
struct Cleanup {
    host: String,
    port: u16,
    unique_id: String,
    /// The host is Sunshine and may hold a pairing session of ours.
    abort_session: bool,
    armed: bool,
}

impl Cleanup {
    fn requests(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let (host, port, abort) = (self.host.clone(), self.port, self.abort_session);
        let id = url_encode(&self.unique_id);
        async move {
            let short = Duration::from_secs(3);
            if abort {
                // Sunshine keeps an unfinished pairing session for five
                // minutes, shows it as waiting on its PIN page, and (tag
                // 2026.914; master replaces it, nvhttp.cpp
                // evict_abandoned_pair_session) answers 409 to a new attempt
                // with the same unique id. A /pair that names no stage makes
                // it drop the session at once (nvhttp.cpp pair: "Invalid
                // pairing request", fail_pair, erase).
                let target = format!("/pair?uniqueid={id}&uuid={}", uuid::Uuid::new_v4());
                let _ = http_get(&host, port, None, &target, "pair (abort)", short).await;
            }
            // libgamestream/client.c gs_pair cleanup: `if (ret != GS_OK) gs_unpair(server)`.
            let target = format!("/unpair?uniqueid={id}&uuid={}", uuid::Uuid::new_v4());
            let _ = http_get(&host, port, None, &target, "unpair", short).await;
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            log::info!("sunshine: pairing abandoned; telling the host");
            runtime.spawn(self.requests());
        }
    }
}

/// `paired` must be 1 in every answer of the exchange
/// (libgamestream/client.c gs_pair: xml_status, then "paired" == "1").
fn expect_paired(root: &XmlNode) -> Result<(), Error> {
    check_status(root)?;
    if root.text_of("paired").trim() != "1" {
        return Err(Error::PairingFailed);
    }
    Ok(())
}

/// Pairs with a host.
///
/// `pin` is the four-digit PIN that the caller shows to the person
/// ([`generate_pin`]); the person types it at the host (Sunshine: its web
/// page, "PIN"). `device_name` is what the host shows for the waiting request.
/// `pin_timeout` bounds the wait for the person ([`DEFAULT_PIN_TIMEOUT`]): the
/// first request stays open at the host until the PIN is entered.
///
/// Cancelling: drop the future. The open connection closes with it, and the
/// host is told to forget the attempt.
///
/// The whole exchange but its last step runs over plain HTTP, as the protocol
/// has it; what protects it is the PIN (it keys the AES challenge) and the
/// signature check on the host's secret.
pub async fn pair(
    host: &str,
    http_port: u16,
    identity: &Identity,
    pin: &str,
    device_name: &str,
    pin_timeout: Duration,
) -> Result<Paired, Error> {
    if pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Internal("the PIN must be four digits"));
    }
    identity.check()?;
    let client = HostClient::new(host, http_port, identity, None)?;
    // libgamestream/client.c gs_init -> load_server_status: the host's
    // generation and HTTPS port are needed first.
    let info = client.server_info_http().await?;
    let is_sunshine = info.state.starts_with("SUNSHINE");
    log::info!(
        "sunshine: pairing with {host}:{} (host version {}); waiting for the PIN to be entered there",
        if http_port == 0 { super::http::DEFAULT_HTTP_PORT } else { http_port },
        info.app_version
    );

    let mut cleanup = Cleanup {
        host: host.trim().to_string(),
        port: if http_port == 0 { super::http::DEFAULT_HTTP_PORT } else { http_port },
        unique_id: identity.unique_id.clone(),
        abort_session: false,
        armed: true,
    };
    let result = exchange(
        &client,
        host,
        http_port,
        info.server_major_version() >= 7,
        info.https_port,
        is_sunshine,
        identity,
        pin,
        device_name,
        pin_timeout,
        &mut cleanup,
    )
    .await;
    match result {
        Ok(paired) => {
            cleanup.armed = false;
            log::info!("sunshine: pairing with {host} is complete");
            Ok(paired)
        }
        Err(e) => {
            log::info!("sunshine: pairing with {host} failed: {e:?}");
            cleanup.armed = false;
            cleanup.requests().await;
            // libgamestream/client.c gs_pair: "If we failed when attempting to
            // pair with a game running, that's likely the issue. Sunshine
            // supports pairing with an active session, but GFE does not."
            if !is_sunshine && info.current_game != 0 {
                return Err(Error::Busy);
            }
            Err(e)
        }
    }
}

/// The stages of libgamestream/client.c gs_pair.
#[allow(clippy::too_many_arguments)]
async fn exchange(
    client: &HostClient,
    host: &str,
    http_port: u16,
    sha256: bool,
    https_port: u16,
    is_sunshine: bool,
    identity: &Identity,
    pin: &str,
    device_name: &str,
    pin_timeout: Duration,
    cleanup: &mut Cleanup,
) -> Result<Paired, Error> {
    let hash_length = if sha256 { 32 } else { 20 };
    // Every request of the exchange starts like this ("devicename=roth&updateState=1" there).
    let common = format!("devicename={}&updateState=1", url_encode(device_name));

    // The certificate goes to the host as the hex of its PEM text
    // (libgamestream/client.c load_cert cert_hex).
    let cert_hex = hex_encode(identity.client_cert_pem.as_bytes());
    let client_cert_signature = cert_der_from_pem(&identity.client_cert_pem)
        .and_then(|der| cert_parts(&der))
        .ok_or(Error::Identity("certificate cannot be read"))?
        .signature;

    // Stage 1: salt and client certificate out, the host's certificate back.
    // The host answers only when the PIN was typed there (Sunshine nvhttp.cpp
    // start_pairing_session keeps the response; pin() writes it).
    let salt = random_bytes::<16>()?;
    cleanup.abort_session = is_sunshine;
    let answer = client
        .get_http(
            "pair",
            &format!("{common}&phrase=getservercert&salt={}&clientcert={cert_hex}", hex_encode(&salt)),
            pin_timeout,
        )
        .await;
    let xml = match answer {
        Ok(xml) => xml,
        Err(Error::Timeout) => return Err(Error::PairingTimeout),
        Err(e) => return Err(e),
    };
    let root = parse_xml(&xml)?;
    // The statuses of Sunshine nvhttp.cpp start_pairing_session,
    // expire_pair_sessions_unlocked and cancel_pairing.
    match xml_status(&root) {
        (409, _) => {
            // Another attempt with this unique id is under way: leave it alone.
            cleanup.abort_session = false;
            return Err(Error::PairingInProgress);
        }
        (408, _) => {
            cleanup.abort_session = false;
            return Err(Error::PairingTimeout);
        }
        (400, m) if m.contains("cancelled") => {
            cleanup.abort_session = false;
            return Err(Error::PairingRefused);
        }
        _ => {}
    }
    expect_paired(&root)?;
    let plaincert_hex = root.text_of("plaincert").trim();
    if plaincert_hex.is_empty() {
        // moonlight-qt nvpairingmanager.cpp: "Server likely already pairing".
        return Err(Error::PairingInProgress);
    }
    let server_cert_pem = hex_decode(plaincert_hex)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or(Error::Protocol("plaincert is not hex of text"))?;
    let server_cert_signature = cert_der_from_pem(&server_cert_pem)
        .and_then(|der| cert_parts(&der))
        .ok_or(Error::Protocol("plaincert is not a certificate"))?
        .signature;
    log::debug!("sunshine: pairing: the host sent its certificate");

    // The AES key from salt and PIN.
    let aes_key = derive_aes_key(&salt, pin, sha256);

    // Stage 2: a random challenge, encrypted.
    let challenge = random_bytes::<16>()?;
    let xml = client
        .get_http(
            "pair",
            &format!("{common}&clientchallenge={}", hex_encode(&aes_ecb_encrypt(&aes_key, &challenge))),
            REQUEST_TIMEOUT,
        )
        .await?;
    let root = parse_xml(&xml)?;
    expect_paired(&root)?;
    let response_enc = hex_decode(root.text_of("challengeresponse").trim())
        .ok_or(Error::Protocol("challengeresponse is not hex"))?;
    if response_enc.len() > 64 {
        // "Server challenge response too big"
        return Err(Error::Protocol("challengeresponse is too long"));
    }
    // hash(challenge + host certificate signature + host secret), then the host's challenge.
    let response = aes_ecb_decrypt(&aes_key, &response_enc);
    if response.len() < hash_length + 16 {
        // moonlight-qt nvpairingmanager.cpp: "Invalid challengeresponse at stage #2"
        return Err(Error::Protocol("challengeresponse is too short"));
    }
    let server_response_hash = &response[..hash_length];
    let server_challenge = &response[hash_length..hash_length + 16];

    // Stage 3: hash(host's challenge + our certificate's signature + our
    // secret), padded to 32 bytes, encrypted.
    let client_secret = random_bytes::<16>()?;
    let mut to_hash = server_challenge.to_vec();
    to_hash.extend_from_slice(&client_cert_signature);
    to_hash.extend_from_slice(&client_secret);
    let mut client_hash = pairing_hash(sha256, &to_hash);
    client_hash.resize(32, 0);
    let xml = client
        .get_http(
            "pair",
            &format!("{common}&serverchallengeresp={}", hex_encode(&aes_ecb_encrypt(&aes_key, &client_hash))),
            REQUEST_TIMEOUT,
        )
        .await?;
    let root = parse_xml(&xml)?;
    expect_paired(&root)?;
    let pairing_secret = hex_decode(root.text_of("pairingsecret").trim())
        .ok_or(Error::Protocol("pairingsecret is not hex"))?;
    if pairing_secret.len() <= 16 {
        return Err(Error::Protocol("pairingsecret is too short"));
    }
    // The host's secret, signed with the key of the certificate it sent.
    let (server_secret, server_signature) = pairing_secret.split_at(16);
    if !rsa_verify_sha256(&server_cert_pem, server_secret, server_signature) {
        // "MITM attack detected"
        log::warn!("sunshine: pairing: the host's secret is not signed by the certificate it showed");
        return Err(Error::PairingFailed);
    }
    // The wrong PIN shows here: the host's hash of stage 2 was made over what
    // it decrypted with its key (moonlight-qt nvpairingmanager.cpp "Incorrect
    // PIN"; the host's side is Sunshine nvhttp.cpp clientchallenge).
    let mut expected = challenge.to_vec();
    expected.extend_from_slice(&server_cert_signature);
    expected.extend_from_slice(server_secret);
    if pairing_hash(sha256, &expected) != server_response_hash {
        return Err(Error::PinRejected);
    }

    // Stage 4: our secret, signed with our key.
    let mut client_pairing_secret = client_secret.to_vec();
    client_pairing_secret.extend_from_slice(&rsa_sign_sha256(&identity.client_key_pem, &client_secret)?);
    let xml = client
        .get_http(
            "pair",
            &format!("{common}&clientpairingsecret={}", hex_encode(&client_pairing_secret)),
            REQUEST_TIMEOUT,
        )
        .await?;
    // With its answer Sunshine has dropped the session, whatever it says
    // (nvhttp.cpp pair: pairing_complete).
    cleanup.abort_session = false;
    let root = parse_xml(&xml)?;
    expect_paired(&root)?;
    log::debug!("sunshine: pairing: the host accepted the client's secret");

    // Stage 5: the first request over HTTPS, with the client certificate. The
    // reference does not check the host's certificate here
    // (libgamestream/http.c); this accepts only the one of stage 1, as
    // moonlight-qt does.
    let paired_client = HostClient::new(host, http_port, identity, Some(&server_cert_pem))?;
    paired_client.set_https_port(https_port);
    let xml = paired_client
        .get_https("pair", &format!("{common}&phrase=pairchallenge"), REQUEST_TIMEOUT)
        .await?;
    let root = parse_xml(&xml)?;
    expect_paired(&root)?;

    Ok(Paired { server_cert_pem })
}

#[cfg(test)]
#[path = "pair_tests.rs"]
mod tests;
