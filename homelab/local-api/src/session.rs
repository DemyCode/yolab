use std::collections::BTreeMap;

use rand::RngExt;
use sha2::{Digest, Sha256};

const TOKEN_VERSION: &str = "v1";

pub fn random_token(len: usize) -> String {
    rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    !hash.is_empty() && pwhash::unix::verify(password, hash)
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner_pad: Vec<u8> = block.iter().map(|b| b ^ 0x36).collect();
    let outer_pad: Vec<u8> = block.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn fingerprint(hash: &str) -> String {
    hex::encode(&Sha256::digest(hash.as_bytes())[..8])
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[derive(Debug, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub expires: i64,
}

pub fn issue(key: &str, password_hash: &str, id: &str, expires: i64) -> String {
    let body = format!(
        "{TOKEN_VERSION}.{expires}.{id}.{}",
        fingerprint(password_hash)
    );
    let mac = hex::encode(hmac_sha256(key.as_bytes(), body.as_bytes()));
    format!("{body}.{mac}")
}

pub fn parse(key: &str, token: &str) -> Option<Session> {
    let (body, mac) = token.rsplit_once('.')?;
    let expected = hex::encode(hmac_sha256(key.as_bytes(), body.as_bytes()));
    if key.is_empty() || !ct_eq(mac, &expected) {
        return None;
    }
    let mut parts = body.split('.');
    let (version, expires, id, _fp) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if version != TOKEN_VERSION || parts.next().is_some() {
        return None;
    }
    Some(Session {
        id: id.to_string(),
        expires: expires.parse().ok()?,
    })
}

pub fn valid(
    key: &str,
    token: &str,
    password_hash: &str,
    revoked: &BTreeMap<String, i64>,
    now: i64,
) -> Option<Session> {
    let session = parse(key, token)?;
    let fp = token.rsplit('.').nth(1)?;
    if password_hash.is_empty()
        || session.expires <= now
        || !ct_eq(fp, &fingerprint(password_hash))
        || revoked.contains_key(&session.id)
    {
        return None;
    }
    Some(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "signing-key";
    const HASH: &str = "$6$machine";

    fn check(token: &str, hash: &str, revoked: &[&str]) -> Option<Session> {
        let revoked = revoked.iter().map(|id| (id.to_string(), 2000)).collect();
        valid(KEY, token, hash, &revoked, 1000)
    }

    #[test]
    fn hmac_matches_rfc_4231_test_case_2() {
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231_test_case_6_with_a_key_longer_than_a_block() {
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn nothing_verifies_against_an_empty_hash() {
        assert!(!verify_password("", ""));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn a_session_issued_on_one_machine_is_accepted_on_another_with_the_same_key() {
        let token = issue(KEY, HASH, "id1", 2000);
        assert_eq!(
            check(&token, HASH, &[]),
            Some(Session {
                id: "id1".into(),
                expires: 2000
            })
        );
    }

    #[test]
    fn a_session_signed_with_another_key_is_refused() {
        let token = issue("other-key", HASH, "id1", 2000);
        assert_eq!(check(&token, HASH, &[]), None);
    }

    #[test]
    fn an_empty_key_never_accepts_a_session() {
        let token = issue("", HASH, "id1", 2000);
        assert_eq!(parse("", &token), None);
    }

    #[test]
    fn a_session_whose_expiry_was_changed_is_refused() {
        let token = issue(KEY, HASH, "id1", 2000).replacen("2000", "9999", 1);
        assert_eq!(check(&token, HASH, &[]), None);
    }

    #[test]
    fn an_expired_session_is_refused() {
        let token = issue(KEY, HASH, "id1", 1000);
        assert_eq!(check(&token, HASH, &[]), None);
    }

    #[test]
    fn changing_the_password_ends_the_sessions_signed_before() {
        let token = issue(KEY, "$6$old", "id1", 2000);
        assert_eq!(check(&token, "$6$new", &[]), None);
    }

    #[test]
    fn without_a_password_no_session_is_accepted() {
        let token = issue(KEY, "", "id1", 2000);
        assert_eq!(check(&token, "", &[]), None);
    }

    #[test]
    fn a_signed_out_session_is_refused() {
        let token = issue(KEY, HASH, "id1", 2000);
        assert_eq!(check(&token, HASH, &["id1"]), None);
        assert!(check(&issue(KEY, HASH, "id2", 2000), HASH, &["id1"]).is_some());
    }

    #[test]
    fn a_token_with_an_extra_part_is_refused() {
        let token = issue(KEY, HASH, "id1", 2000);
        let (body, _) = token.rsplit_once('.').unwrap();
        let forged_body = format!("{body}.extra");
        let mac = hex::encode(hmac_sha256(KEY.as_bytes(), forged_body.as_bytes()));
        assert_eq!(parse(KEY, &format!("{forged_body}.{mac}")), None);
    }
}
