use jsonwebtoken::{encode, decode, Header, Algorithm, Validation, EncodingKey, DecodingKey};
use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    access_key: String,
    iat: u64,
    exp: u64,
}

const TOKEN_LIFETIME_SECS: u64 = 900; // 15 minutes
const MAX_CLOCK_SKEW_SECS: u64 = 900; // 15 minutes

pub fn sign_token(access_key: &str, secret_key: &str) -> Result<String, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = Claims {
        access_key: access_key.to_string(),
        iat: now,
        exp: now + TOKEN_LIFETIME_SECS,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret_key.as_bytes()),
    )
    .map_err(|e| format!("jwt sign: {}", e))
}

pub fn validate_token(
    token: &str,
    expected_access_key: &str,
    secret_key: &str,
) -> Result<(), String> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_required_spec_claims(&["exp"]);

    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret_key.as_bytes()),
        &validation,
    )
    .map_err(|e| format!("jwt validate: {}", e))?;

    if data.claims.access_key != expected_access_key {
        return Err("access key mismatch".to_string());
    }

    Ok(())
}

pub fn validate_clock_skew(remote_nanos: &str) -> Result<(), String> {
    let remote: u64 = remote_nanos.parse().map_err(|_| "invalid timestamp")?;
    let local = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let diff = if remote > local {
        remote - local
    } else {
        local - remote
    };
    let diff_secs = diff / 1_000_000_000;
    if diff_secs > MAX_CLOCK_SKEW_SECS {
        return Err(format!("clock skew {}s exceeds {}s limit", diff_secs, MAX_CLOCK_SKEW_SECS));
    }
    Ok(())
}

pub fn current_time_nanos() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_validate_round_trip() {
        let token = sign_token("mykey", "mysecret").unwrap();
        assert!(validate_token(&token, "mykey", "mysecret").is_ok());
    }

    #[test]
    fn validate_rejects_wrong_secret() {
        let token = sign_token("mykey", "mysecret").unwrap();
        assert!(validate_token(&token, "mykey", "wrong").is_err());
    }

    #[test]
    fn validate_rejects_wrong_access_key() {
        let token = sign_token("mykey", "mysecret").unwrap();
        assert!(validate_token(&token, "otherkey", "mysecret").is_err());
    }

    #[test]
    fn clock_skew_within_limit() {
        let nanos = current_time_nanos();
        assert!(validate_clock_skew(&nanos).is_ok());
    }

    #[test]
    fn clock_skew_rejects_stale() {
        assert!(validate_clock_skew("0").is_err());
    }
}
