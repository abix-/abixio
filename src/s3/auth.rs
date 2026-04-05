use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub struct AuthConfig {
    pub access_key: String,
    pub secret_key: String,
    pub no_auth: bool,
}

/// Verify AWS Signature V4 on an HTTP request.
/// Returns Ok(()) if valid, Err with reason if not.
#[allow(clippy::too_many_arguments)]
pub fn verify_sig_v4(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    body_hash: &str,
    auth_header: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<(), String> {
    // parse Authorization header
    // AWS4-HMAC-SHA256 Credential=<key>/<date>/<region>/s3/aws4_request,
    //   SignedHeaders=<headers>, Signature=<sig>
    let auth = auth_header
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or("missing AWS4-HMAC-SHA256 prefix")?;

    let mut credential = "";
    let mut signed_headers_str = "";
    let mut provided_sig = "";

    for part in auth.split(", ") {
        if let Some(val) = part.strip_prefix("Credential=") {
            credential = val;
        } else if let Some(val) = part.strip_prefix("SignedHeaders=") {
            signed_headers_str = val;
        } else if let Some(val) = part.strip_prefix("Signature=") {
            provided_sig = val;
        }
    }

    // parse credential: access_key/date/region/s3/aws4_request
    let cred_parts: Vec<&str> = credential.split('/').collect();
    if cred_parts.len() != 5 {
        return Err("invalid credential format".to_string());
    }
    let req_access_key = cred_parts[0];
    let date = cred_parts[1];
    let region = cred_parts[2];
    // cred_parts[3] = "s3", cred_parts[4] = "aws4_request"

    if req_access_key != access_key {
        return Err("access key mismatch".to_string());
    }

    // check timestamp (X-Amz-Date header)
    let amz_date = headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "x-amz-date")
        .map(|(_, v)| v.as_str())
        .ok_or("missing X-Amz-Date header")?;

    // check clock skew (>15 min)
    if let Ok(req_time) = parse_amz_date(amz_date) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let diff = req_time.abs_diff(now);
        if diff > 15 * 60 {
            return Err("request time too skewed".to_string());
        }
    }

    // build canonical request
    let signed_headers: Vec<&str> = signed_headers_str.split(';').collect();
    let canonical_headers = build_canonical_headers(headers, &signed_headers);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        canonical_uri(path),
        canonical_query(query),
        canonical_headers,
        signed_headers_str,
        body_hash
    );

    // string to sign
    let credential_scope = format!("{}/{}/s3/aws4_request", date, region);
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date, credential_scope, canonical_request_hash
    );

    // derive signing key
    let signing_key = derive_signing_key(secret_key, date, region);

    // compute signature
    let computed_sig = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

    // constant-time compare
    if computed_sig
        .as_bytes()
        .ct_eq(provided_sig.as_bytes())
        .into()
    {
        Ok(())
    } else {
        Err("signature mismatch".to_string())
    }
}

/// Check if a query string contains presigned URL parameters.
pub fn is_presigned(query: &str) -> bool {
    query.contains("X-Amz-Algorithm") || query.contains("X-Amz-Credential")
}

/// Verify AWS Signature V4 presigned URL.
/// Query params: X-Amz-Algorithm, X-Amz-Credential, X-Amz-Date, X-Amz-Expires,
///               X-Amz-SignedHeaders, X-Amz-Signature
#[allow(clippy::too_many_arguments)]
pub fn verify_presigned_v4(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    access_key: &str,
    secret_key: &str,
) -> Result<(), String> {
    let params = parse_query_params(query);

    let algorithm = params.get("X-Amz-Algorithm").ok_or("missing X-Amz-Algorithm")?;
    if *algorithm != "AWS4-HMAC-SHA256" {
        return Err("unsupported algorithm".to_string());
    }

    let credential = params.get("X-Amz-Credential").ok_or("missing X-Amz-Credential")?;
    let amz_date = params.get("X-Amz-Date").ok_or("missing X-Amz-Date")?;
    let expires_str = params.get("X-Amz-Expires").ok_or("missing X-Amz-Expires")?;
    let signed_headers_str = params.get("X-Amz-SignedHeaders").ok_or("missing X-Amz-SignedHeaders")?;
    let provided_sig = params.get("X-Amz-Signature").ok_or("missing X-Amz-Signature")?;

    // parse credential: access_key/date/region/s3/aws4_request
    let cred_parts: Vec<&str> = credential.split('/').collect();
    if cred_parts.len() != 5 {
        return Err("invalid credential format".to_string());
    }
    let req_access_key = cred_parts[0];
    let date = cred_parts[1];
    let region = cred_parts[2];

    if req_access_key != access_key {
        return Err("access key mismatch".to_string());
    }

    // parse and check date
    let req_time = parse_amz_date(amz_date).map_err(|_| "invalid X-Amz-Date".to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // check not in the future by more than 15 min
    if req_time > now + 15 * 60 {
        return Err("request date is in the future".to_string());
    }

    // check expiration
    let expires: u64 = expires_str.parse().map_err(|_| "invalid X-Amz-Expires".to_string())?;
    if expires > 604800 {
        return Err("expires too large (max 7 days)".to_string());
    }
    if now > req_time + expires {
        return Err("presigned URL has expired".to_string());
    }

    // build canonical query string (exclude X-Amz-Signature)
    let canonical_qs = canonical_query_presigned(query);

    // build canonical headers
    let signed_headers: Vec<&str> = signed_headers_str.split(';').collect();
    let canonical_hdrs = build_canonical_headers(headers, &signed_headers);

    // build canonical request
    // presigned URLs use UNSIGNED-PAYLOAD
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        canonical_uri(path),
        canonical_qs,
        canonical_hdrs,
        signed_headers_str,
        "UNSIGNED-PAYLOAD"
    );

    // string to sign
    let credential_scope = format!("{}/{}/s3/aws4_request", date, region);
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date, credential_scope, canonical_request_hash
    );

    // derive signing key and compute signature
    let signing_key = derive_signing_key(secret_key, date, region);
    let computed_sig = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

    // constant-time compare
    if computed_sig
        .as_bytes()
        .ct_eq(provided_sig.as_bytes())
        .into()
    {
        Ok(())
    } else {
        Err("signature mismatch".to_string())
    }
}

fn parse_query_params(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if query.is_empty() {
        return map;
    }
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(
                percent_decode(k),
                percent_decode(v),
            );
        }
    }
    map
}

fn percent_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(result).unwrap_or_default()
}

/// Build canonical query string for presigned URLs, excluding X-Amz-Signature.
fn canonical_query_presigned(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter_map(|p| p.split_once('=').or(Some((p, ""))))
        .filter(|(k, _)| *k != "X-Amz-Signature")
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&")
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    hex::encode(hmac_sha256(key, data))
}

fn derive_signing_key(secret_key: &str, date: &str, region: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{}", secret_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    hmac_sha256(&k_service, b"aws4_request")
}

fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    }
}

fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter_map(|p| p.split_once('=').or(Some((p, ""))))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&")
}

fn build_canonical_headers(headers: &[(String, String)], signed: &[&str]) -> String {
    let mut result = String::new();
    for name in signed {
        if let Some((_, val)) = headers.iter().find(|(k, _)| k.to_lowercase() == *name) {
            result.push_str(&format!("{}:{}\n", name, val.trim()));
        }
    }
    result
}

#[allow(clippy::result_unit_err)]
fn parse_amz_date(date: &str) -> Result<u64, ()> {
    // format: 20240101T000000Z
    if date.len() != 16 {
        return Err(());
    }
    // rough parse: extract year/month/day/hour/min/sec
    let year: u64 = date[0..4].parse().map_err(|_| ())?;
    let month: u64 = date[4..6].parse().map_err(|_| ())?;
    let day: u64 = date[6..8].parse().map_err(|_| ())?;
    let hour: u64 = date[9..11].parse().map_err(|_| ())?;
    let min: u64 = date[11..13].parse().map_err(|_| ())?;
    let sec: u64 = date[13..15].parse().map_err(|_| ())?;

    // approximate unix timestamp (good enough for 15min skew check)
    let days = (year - 1970) * 365 + (year - 1969) / 4 + month_days(month) + day - 1;
    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn month_days(month: u64) -> u64 {
    const DAYS: [u64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    if (1..=12).contains(&month) {
        DAYS[month as usize - 1]
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // helper: sign a request and return the Authorization header
    fn sign_request(
        method: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
        body: &[u8],
        access_key: &str,
        secret_key: &str,
        date: &str,
        amz_date: &str,
        region: &str,
    ) -> String {
        let body_hash = sha256_hex(body);
        let signed_headers_list = {
            let mut names: Vec<String> = headers.iter().map(|(k, _)| k.to_lowercase()).collect();
            names.sort();
            names.join(";")
        };
        let signed_header_names: Vec<&str> = signed_headers_list.split(';').collect();

        let canonical_hdrs = build_canonical_headers(headers, &signed_header_names);
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method,
            canonical_uri(path),
            canonical_query(query),
            canonical_hdrs,
            signed_headers_list,
            body_hash
        );

        let credential_scope = format!("{}/{}/s3/aws4_request", date, region);
        let cr_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date, credential_scope, cr_hash
        );

        let signing_key = derive_signing_key(secret_key, date, region);
        let signature = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            access_key, credential_scope, signed_headers_list, signature
        )
    }

    fn current_amz_timestamps() -> (String, String) {
        // use chrono-free approach: format current time
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // approximate: good enough for tests
        let secs_in_day = 86400u64;
        let days_since_epoch = now / secs_in_day;
        let remaining = now % secs_in_day;
        let hour = remaining / 3600;
        let min = (remaining % 3600) / 60;
        let sec = remaining % 60;

        // approximate year/month/day
        let mut year = 1970u64;
        let mut remaining_days = days_since_epoch;
        loop {
            let days_in_year = if year.is_multiple_of(4) { 366 } else { 365 };
            if remaining_days < days_in_year {
                break;
            }
            remaining_days -= days_in_year;
            year += 1;
        }
        let month_lengths = if year.is_multiple_of(4) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        let mut month = 1u64;
        for &ml in &month_lengths {
            if remaining_days < ml {
                break;
            }
            remaining_days -= ml;
            month += 1;
        }
        let day = remaining_days + 1;

        let date = format!("{:04}{:02}{:02}", year, month, day);
        let amz_date = format!("{}T{:02}{:02}{:02}Z", date, hour, min, sec);
        (date, amz_date)
    }

    #[test]
    fn verify_valid_signature() {
        let (date, amz_date) = current_amz_timestamps();
        let headers = vec![
            ("host".to_string(), "localhost:9000".to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
            ("x-amz-content-sha256".to_string(), sha256_hex(b"")),
        ];
        let auth = sign_request(
            "GET",
            "/test/key",
            "",
            &headers,
            b"",
            "myaccesskey",
            "mysecretkey",
            &date,
            &amz_date,
            "us-east-1",
        );
        let result = verify_sig_v4(
            "GET",
            "/test/key",
            "",
            &headers,
            &sha256_hex(b""),
            &auth,
            "myaccesskey",
            "mysecretkey",
        );
        assert!(result.is_ok(), "expected ok, got: {:?}", result);
    }

    #[test]
    fn verify_rejects_wrong_access_key() {
        let (date, amz_date) = current_amz_timestamps();
        let headers = vec![
            ("host".to_string(), "localhost:9000".to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
            ("x-amz-content-sha256".to_string(), sha256_hex(b"")),
        ];
        let auth = sign_request(
            "GET",
            "/",
            "",
            &headers,
            b"",
            "wrongkey",
            "mysecretkey",
            &date,
            &amz_date,
            "us-east-1",
        );
        let result = verify_sig_v4(
            "GET",
            "/",
            "",
            &headers,
            &sha256_hex(b""),
            &auth,
            "myaccesskey",
            "mysecretkey",
        );
        assert!(result.is_err());
    }

    #[test]
    fn verify_rejects_wrong_secret_key() {
        let (date, amz_date) = current_amz_timestamps();
        let headers = vec![
            ("host".to_string(), "localhost:9000".to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
            ("x-amz-content-sha256".to_string(), sha256_hex(b"")),
        ];
        let auth = sign_request(
            "GET",
            "/",
            "",
            &headers,
            b"",
            "myaccesskey",
            "wrongsecret",
            &date,
            &amz_date,
            "us-east-1",
        );
        let result = verify_sig_v4(
            "GET",
            "/",
            "",
            &headers,
            &sha256_hex(b""),
            &auth,
            "myaccesskey",
            "mysecretkey",
        );
        assert!(result.is_err());
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let (date, amz_date) = current_amz_timestamps();
        let headers = vec![
            ("host".to_string(), "localhost:9000".to_string()),
            ("x-amz-date".to_string(), amz_date.clone()),
            ("x-amz-content-sha256".to_string(), sha256_hex(b"original")),
        ];
        let auth = sign_request(
            "PUT",
            "/test/key",
            "",
            &headers,
            b"original",
            "myaccesskey",
            "mysecretkey",
            &date,
            &amz_date,
            "us-east-1",
        );
        // verify with different body hash
        let result = verify_sig_v4(
            "PUT",
            "/test/key",
            "",
            &headers,
            &sha256_hex(b"tampered"),
            &auth,
            "myaccesskey",
            "mysecretkey",
        );
        assert!(result.is_err());
    }

    #[test]
    fn verify_rejects_expired_request() {
        // use a date far in the past
        let date = "20200101";
        let amz_date = "20200101T000000Z";
        let headers = vec![
            ("host".to_string(), "localhost:9000".to_string()),
            ("x-amz-date".to_string(), amz_date.to_string()),
            ("x-amz-content-sha256".to_string(), sha256_hex(b"")),
        ];
        let auth = sign_request(
            "GET",
            "/",
            "",
            &headers,
            b"",
            "myaccesskey",
            "mysecretkey",
            date,
            amz_date,
            "us-east-1",
        );
        let result = verify_sig_v4(
            "GET",
            "/",
            "",
            &headers,
            &sha256_hex(b""),
            &auth,
            "myaccesskey",
            "mysecretkey",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("skew"));
    }

    // helper: build presigned URL query string
    fn presign_query(
        method: &str,
        path: &str,
        access_key: &str,
        secret_key: &str,
        date: &str,
        amz_date: &str,
        region: &str,
        expires: u64,
    ) -> String {
        let credential = format!("{}/{}/{}/s3/aws4_request", access_key, date, region);
        let signed_headers = "host";

        // build canonical query (without signature, sorted)
        let mut query_parts = vec![
            format!("X-Amz-Algorithm=AWS4-HMAC-SHA256"),
            format!("X-Amz-Credential={}", credential),
            format!("X-Amz-Date={}", amz_date),
            format!("X-Amz-Expires={}", expires),
            format!("X-Amz-SignedHeaders={}", signed_headers),
        ];
        query_parts.sort();
        let canonical_qs = query_parts.join("&");

        let headers = vec![("host".to_string(), "localhost:9000".to_string())];
        let canonical_hdrs = build_canonical_headers(&headers, &["host"]);
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method,
            canonical_uri(path),
            canonical_qs,
            canonical_hdrs,
            signed_headers,
            "UNSIGNED-PAYLOAD"
        );

        let credential_scope = format!("{}/{}/s3/aws4_request", date, region);
        let cr_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date, credential_scope, cr_hash
        );

        let signing_key = derive_signing_key(secret_key, date, region);
        let signature = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        format!("{}&X-Amz-Signature={}", canonical_qs, signature)
    }

    #[test]
    fn verify_presigned_valid() {
        let (date, amz_date) = current_amz_timestamps();
        let query = presign_query(
            "GET", "/test/key", "myaccesskey", "mysecretkey",
            &date, &amz_date, "us-east-1", 3600,
        );
        let headers = vec![("host".to_string(), "localhost:9000".to_string())];
        let result = verify_presigned_v4(
            "GET", "/test/key", &query, &headers,
            "myaccesskey", "mysecretkey",
        );
        assert!(result.is_ok(), "expected ok, got: {:?}", result);
    }

    #[test]
    fn verify_presigned_expired() {
        // use a date far in the past with 1 second expiry
        let date = "20200101";
        let amz_date = "20200101T000000Z";
        let query = presign_query(
            "GET", "/test/key", "myaccesskey", "mysecretkey",
            date, amz_date, "us-east-1", 1,
        );
        let headers = vec![("host".to_string(), "localhost:9000".to_string())];
        let result = verify_presigned_v4(
            "GET", "/test/key", &query, &headers,
            "myaccesskey", "mysecretkey",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expired"));
    }

    #[test]
    fn verify_presigned_bad_key() {
        let (date, amz_date) = current_amz_timestamps();
        let query = presign_query(
            "GET", "/test/key", "myaccesskey", "mysecretkey",
            &date, &amz_date, "us-east-1", 3600,
        );
        let headers = vec![("host".to_string(), "localhost:9000".to_string())];
        let result = verify_presigned_v4(
            "GET", "/test/key", &query, &headers,
            "wrongkey", "mysecretkey",
        );
        assert!(result.is_err());
    }

    #[test]
    fn is_presigned_detects_params() {
        assert!(is_presigned("X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=key"));
        assert!(!is_presigned("prefix=foo&delimiter=/"));
    }
}
