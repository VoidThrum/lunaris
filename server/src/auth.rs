use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

/// The secret older builds fell back to; anyone could sign tokens with it.
const OLD_DEFAULT_SECRET: &str = "lunaris-dev-secret-change-in-production";

static JWT_SECRET: Lazy<Vec<u8>> = Lazy::new(|| {
    #[cfg(test)]
    {
        crate::security::random_hex(32).into_bytes()
    }
    #[cfg(not(test))]
    {
        choose_secret(std::env::var("JWT_SECRET").ok(), &crate::paths::data_file("jwt_secret.txt"))
    }
});

/// `JWT_SECRET` when set (never the old default), otherwise a random secret
/// kept in `file` so sign-ins survive a restart.
fn choose_secret(env: Option<String>, file: &std::path::Path) -> Vec<u8> {
    if let Some(secret) = env.map(|s| s.trim().to_string()) {
        if !secret.is_empty() && secret != OLD_DEFAULT_SECRET {
            return secret.into_bytes();
        }
        if secret == OLD_DEFAULT_SECRET {
            tracing::warn!("JWT_SECRET is the old public default; ignoring it");
        }
    }
    if let Ok(saved) = std::fs::read_to_string(file) {
        let saved = saved.trim();
        if saved.len() >= 32 && saved != OLD_DEFAULT_SECRET {
            return saved.as_bytes().to_vec();
        }
    }
    let secret = crate::security::random_hex(32);
    if let Err(e) = crate::paths::write_private(file, &secret) {
        tracing::error!("Cannot save the sign-in key to {:?} (sign-ins end at restart): {}", file, e);
    }
    secret.into_bytes()
}

/// A valid bcrypt hash (cost 12) checked for unknown usernames, so they take
/// as long as wrong passwords.
pub const DUMMY_HASH: &str = "$2b$12$rhTXdG3k9pjCa6u1.pNncOA9kj.STXQU8E5kun0D6dJtQRVnVtVR.";

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // User ID
    pub username: String,
    pub role: String,
    pub exp: u64,
}

pub fn hash_password(password: &str) -> Result<String, bcrypt::BcryptError> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST)
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// bcrypt is slow on purpose: run it off the async workers.
pub async fn hash_password_blocking(password: String) -> Result<String, String> {
    match tokio::task::spawn_blocking(move || hash_password(&password)).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

pub async fn verify_password_blocking(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .unwrap_or(false)
}

pub fn create_jwt(
    user_id: &str,
    username: &str,
    role: &str,
) -> Result<String, jsonwebtoken::errors::Error> {
    let expiration = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::days(7))
        .expect("valid timestamp")
        .timestamp() as u64;

    let claims = Claims {
        sub: user_id.to_string(),
        username: username.to_string(),
        role: role.to_string(),
        exp: expiration,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(&JWT_SECRET),
    )
}

pub fn verify_jwt(token: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(&JWT_SECRET),
        &Validation::default(),
    )?;
    Ok(token_data.claims)
}

use crate::signaling::SignalingState;
use std::sync::Arc;

#[allow(dead_code)]
pub struct AuthenticatedUser {
    pub user_id: String,
    pub username: String,
    pub role: String,
}

impl FromRequestParts<Arc<SignalingState>> for AuthenticatedUser {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<SignalingState>,
    ) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|value| value.to_str().ok())
            .ok_or((StatusCode::UNAUTHORIZED, "Missing Authorization Header"))?;

        if !auth_header.starts_with("Bearer ") {
            return Err((
                StatusCode::UNAUTHORIZED,
                "Invalid Authorization Header Format",
            ));
        }

        let token = &auth_header[7..];
        let claims = verify_jwt(token).map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid Token"))?;

        // The user must still exist and be active; the role comes from the database.
        let status = crate::security::user_status(&state.db, &claims.sub)
            .await
            .map_err(|e| {
                tracing::error!("Database verification error: {:?}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Database Error")
            })?;

        match status {
            Some((role, true)) => Ok(AuthenticatedUser {
                user_id: claims.sub,
                username: claims.username,
                role,
            }),
            Some((_, false)) => Err((StatusCode::UNAUTHORIZED, "Account is disabled")),
            None => Err((StatusCode::UNAUTHORIZED, "User does not exist in database")),
        }
    }
}

#[allow(dead_code)]
pub struct AdminUser {
    pub user_id: String,
    pub username: String,
}

impl FromRequestParts<Arc<SignalingState>> for AdminUser {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<SignalingState>,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthenticatedUser::from_request_parts(parts, state).await?;
        if user.role != "admin" {
            return Err((StatusCode::FORBIDDEN, "Admin access required"));
        }
        Ok(AdminUser {
            user_id: user.user_id,
            username: user.username,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hash produced by bcrypt 0.15 (before the upgrade) for "correct horse".
    const OLD_HASH: &str = "$2b$12$rhTXdG3k9pjCa6u1.pNncOA9kj.STXQU8E5kun0D6dJtQRVnVtVR.";

    #[test]
    fn old_bcrypt_hash_still_verifies() {
        assert!(verify_password("correct horse", OLD_HASH));
        assert!(!verify_password("wrong horse", OLD_HASH));
    }

    #[test]
    fn new_hash_roundtrip_and_format() {
        let h = hash_password("pw").unwrap();
        assert!(h.starts_with("$2b$12$"), "{h}");
        assert!(verify_password("pw", &h));
    }

    #[test]
    fn token_signed_with_old_default_secret_is_rejected() {
        let claims = Claims {
            sub: "id1".into(),
            username: "admin".into(),
            role: "admin".into(),
            exp: (chrono::Utc::now().timestamp() + 3600) as u64,
        };
        let forged = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(OLD_DEFAULT_SECRET.as_bytes()),
        )
        .unwrap();
        assert!(verify_jwt(&forged).is_err());
    }

    #[test]
    fn secret_never_the_old_default() {
        let dir = std::env::temp_dir().join(format!("lunaris-jwt-{}", crate::security::random_hex(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("jwt_secret.txt");
        // The old default in the environment is ignored; a random one is saved.
        let s1 = choose_secret(Some(OLD_DEFAULT_SECRET.into()), &file);
        assert_ne!(s1, OLD_DEFAULT_SECRET.as_bytes());
        assert_eq!(s1.len(), 64);
        // Kept across restarts.
        assert_eq!(choose_secret(None, &file), s1);
        // An explicit secret wins.
        assert_eq!(choose_secret(Some("my-own-secret".into()), &file), b"my-own-secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dummy_hash_is_valid_bcrypt() {
        assert!(!verify_password("anything", DUMMY_HASH));
        assert!(bcrypt::verify("x", DUMMY_HASH).is_ok());
    }

    #[test]
    fn jwt_roundtrip_hs256() {
        let t = create_jwt("id1", "bob", "admin").unwrap();
        let header = jsonwebtoken::decode_header(&t).unwrap();
        assert_eq!(header.alg, jsonwebtoken::Algorithm::HS256);
        let c = verify_jwt(&t).unwrap();
        assert_eq!(c.sub, "id1");
    }
}
