//! RustDesk's password login: the host sends a salt and a challenge
//! (`Hash`), the client answers with
//! `SHA-256(SHA-256(password ‖ salt) ‖ challenge)` in `LoginRequest.password`.
//!
//! Ported from rustdesk/src/client.rs `handle_login_from_ui` / `handle_hash`
//! (the client's side) and rustdesk/src/server/connection.rs
//! `validate_password_plain` / `verify_h1` (the host's side).

use crate::direct::handshake::{digest_eq, sha256, Digest};

// rustdesk/src/client.rs: the texts of `LoginResponse.error` the client
// recognises (it asks for the password again on LOGIN_MSG_PASSWORD_WRONG).
pub const LOGIN_MSG_PASSWORD_WRONG: &str = "Wrong Password";
pub const LOGIN_MSG_NO_PASSWORD_ACCESS: &str = "No Password Access";
pub const LOGIN_MSG_OFFLINE: &str = "Offline";
/// rustdesk/src/server/connection.rs `check_failure_with_scope`: said to an
/// address that failed too often.
pub const LOGIN_MSG_TRY_LATER: &str = "Please try 1 minute later";

/// `SHA-256(password ‖ salt)`: what the client keeps when it remembers a
/// password (rustdesk/src/client.rs `handle_login_from_ui`).
pub fn h1(password: &str, salt: &str) -> Digest {
    sha256(&[password.as_bytes(), salt.as_bytes()].concat())
}

/// `SHA-256(h1 ‖ challenge)`: what the client sends as
/// `LoginRequest.password` (rustdesk/src/client.rs `handle_login_from_ui`,
/// `handle_hash`).
pub fn proof(h1: &Digest, challenge: &str) -> Digest {
    sha256(&[&h1[..], challenge.as_bytes()].concat())
}

/// Whether `presented` (`LoginRequest.password`) proves knowledge of
/// `password` for this connection's salt and challenge
/// (rustdesk/src/server/connection.rs `validate_password_plain` and
/// `verify_h1`; compared in constant time as there).
pub fn verify(password: &str, salt: &str, challenge: &str, presented: &[u8]) -> bool {
    if password.is_empty() {
        return false;
    }
    let Ok(presented) = <&Digest>::try_from(presented) else {
        return false;
    };
    digest_eq(&proof(&h1(password, salt), challenge), presented)
}

/// The salt sent in `Hash`. RustDesk keeps one random salt per password
/// (hbb_common `Config::get_effective_permanent_password_salt`); a client that
/// remembers the password stores `h1`, so the salt must stay the same between
/// connections. It is public (sent to whoever connects): here it is derived
/// from the agent's id, so nothing new has to be stored.
pub fn salt_for(agent_id: &str) -> String {
    let digest = sha256(&[b"lunaris-rustdesk-salt:", agent_id.as_bytes()].concat());
    crate::pairing::to_hex(&digest[..8])
}

/// A fresh challenge for one connection (RustDesk: `Config::get_auto_password(6)`
/// in rustdesk/src/server/connection.rs `Connection::start`; the client hashes
/// it as an opaque string, so a longer random one is used here).
pub fn new_challenge() -> String {
    crate::pairing::to_hex(&crate::pairing::random_bytes(16))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        crate::pairing::to_hex(bytes)
    }

    // Vectors computed outside this code (Python hashlib) from the formula in
    // rustdesk/src/client.rs `handle_login_from_ui`:
    //   h1    = sha256(b"hunter2" + b"abc123")
    //   proof = sha256(h1 + b"XyZ789")
    #[test]
    fn matches_the_reference_hashes() {
        let h1 = h1("hunter2", "abc123");
        assert_eq!(hex(&h1), H1_VECTOR);
        assert_eq!(hex(&proof(&h1, "XyZ789")), PROOF_VECTOR);
    }

    const H1_VECTOR: &str = "8065646abe06defef2e2d2d04371bfd1e2eba8feee7ddaf942eb1f3644439161";
    const PROOF_VECTOR: &str = "5d0aee612c332a4be799a69e561c7b8d0b077b26ada368c8c4b5efc1bbc00aa9";

    #[test]
    fn only_the_right_password_for_this_challenge_verifies() {
        let good = proof(&h1("hunter2", "abc123"), "XyZ789");
        assert!(verify("hunter2", "abc123", "XyZ789", &good));
        assert!(!verify("hunter3", "abc123", "XyZ789", &good));
        // A proof replayed against another challenge or salt.
        assert!(!verify("hunter2", "abc123", "XyZ780", &good));
        assert!(!verify("hunter2", "abc124", "XyZ789", &good));
        // Wrong lengths, and the empty password a client sends before asking.
        assert!(!verify("hunter2", "abc123", "XyZ789", &good[..31]));
        assert!(!verify("hunter2", "abc123", "XyZ789", &[]));
        assert!(!verify("", "abc123", "XyZ789", &proof(&h1("", "abc123"), "XyZ789")));
    }

    #[test]
    fn the_salt_is_stable_and_the_challenge_is_not() {
        assert_eq!(salt_for("agent-1"), salt_for("agent-1"));
        assert_ne!(salt_for("agent-1"), salt_for("agent-2"));
        assert_eq!(salt_for("agent-1").len(), 16);
        assert_ne!(new_challenge(), new_challenge());
    }
}
