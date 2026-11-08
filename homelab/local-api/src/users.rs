use std::collections::BTreeMap;

use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ADMIN: &str = "admin";
pub const MIN_PASSWORD_LEN: usize = 8;
const HASH_ROUNDS: u32 = 200_000;
const TOKEN_VERSION: &str = "v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Account {
    Active { hash: String },
    Removed,
}

pub fn random_token(len: usize) -> String {
    rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 32
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let setting = format!("$6$rounds={HASH_ROUNDS}${}", random_token(16));
    pwhash::sha512_crypt::hash_with(setting.as_str(), password)
        .map_err(|e| anyhow::anyhow!("the password could not be hashed: {e}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    !hash.is_empty() && pwhash::unix::verify(password, hash)
}

pub fn active(
    stored: &BTreeMap<String, Account>,
    legacy_admin_hash: &str,
) -> BTreeMap<String, String> {
    let found: BTreeMap<String, String> = stored
        .iter()
        .filter_map(|(name, account)| match account {
            Account::Active { hash } => Some((name.clone(), hash.clone())),
            Account::Removed => None,
        })
        .collect();
    if found.is_empty() && !legacy_admin_hash.is_empty() {
        return BTreeMap::from([(ADMIN.to_string(), legacy_admin_hash.to_string())]);
    }
    found
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
    pub user: String,
    pub id: String,
    pub expires: i64,
}

pub fn issue(key: &str, user: &str, hash: &str, id: &str, expires: i64) -> String {
    let body = format!(
        "{TOKEN_VERSION}.{user}.{expires}.{id}.{}",
        fingerprint(hash)
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
    let (version, user, expires, id, _fp) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if version != TOKEN_VERSION || parts.next().is_some() {
        return None;
    }
    Some(Session {
        user: user.to_string(),
        id: id.to_string(),
        expires: expires.parse().ok()?,
    })
}

pub fn signed_in(
    key: &str,
    token: &str,
    accounts: &BTreeMap<String, String>,
    revoked: &BTreeMap<String, i64>,
    now: i64,
) -> Option<Session> {
    let session = parse(key, token)?;
    let hash = accounts.get(&session.user)?;
    let fp = token.rsplit('.').nth(1)?;
    if session.expires <= now || !ct_eq(fp, &fingerprint(hash)) || revoked.contains_key(&session.id)
    {
        return None;
    }
    Some(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "signing-key";

    fn accounts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(u, h)| (u.to_string(), h.to_string()))
            .collect()
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
    fn a_hashed_password_verifies_and_a_wrong_one_does_not() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$6$rounds=200000$"));
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("wrong horse", &hash));
    }

    #[test]
    fn hashing_the_same_password_twice_gives_different_hashes() {
        assert_ne!(
            hash_password("same").unwrap(),
            hash_password("same").unwrap()
        );
    }

    #[test]
    fn nothing_verifies_against_an_empty_hash() {
        assert!(!verify_password("", ""));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn usernames_are_short_lowercase_words_without_dots() {
        assert!(valid_username("alice"));
        assert!(valid_username("bob-2"));
        assert!(valid_username("a_b"));
        assert!(!valid_username(""));
        assert!(!valid_username("Alice"));
        assert!(!valid_username("a.b"));
        assert!(!valid_username("-a"));
        assert!(!valid_username(&"a".repeat(33)));
    }

    #[test]
    fn with_no_account_stored_the_machine_password_is_the_admin() {
        assert_eq!(
            active(&BTreeMap::new(), "$6$legacy"),
            accounts(&[(ADMIN, "$6$legacy")])
        );
        assert!(active(&BTreeMap::new(), "").is_empty());
    }

    #[test]
    fn once_accounts_are_stored_the_machine_password_is_no_longer_used() {
        let stored = BTreeMap::from([(
            "alice".to_string(),
            Account::Active {
                hash: "$6$alice".into(),
            },
        )]);
        assert_eq!(
            active(&stored, "$6$legacy"),
            accounts(&[("alice", "$6$alice")])
        );
    }

    #[test]
    fn when_every_stored_account_was_removed_the_machine_password_signs_in_again() {
        let stored = BTreeMap::from([("alice".to_string(), Account::Removed)]);
        assert_eq!(
            active(&stored, "$6$legacy"),
            accounts(&[(ADMIN, "$6$legacy")])
        );
        assert!(active(&stored, "").is_empty());
    }

    #[test]
    fn a_removed_account_cannot_sign_in() {
        let stored = BTreeMap::from([
            ("alice".to_string(), Account::Removed),
            (
                "bob".to_string(),
                Account::Active {
                    hash: "$6$bob".into(),
                },
            ),
        ]);
        assert_eq!(active(&stored, ""), accounts(&[("bob", "$6$bob")]));
    }

    #[test]
    fn a_session_issued_on_one_machine_is_accepted_on_another_with_the_same_key() {
        let token = issue(KEY, "alice", "$6$alice", "id1", 2000);
        let found = signed_in(
            KEY,
            &token,
            &accounts(&[("alice", "$6$alice")]),
            &BTreeMap::new(),
            1000,
        );
        assert_eq!(
            found,
            Some(Session {
                user: "alice".into(),
                id: "id1".into(),
                expires: 2000
            })
        );
    }

    #[test]
    fn a_session_signed_with_another_key_is_refused() {
        let token = issue("other-key", "alice", "$6$alice", "id1", 2000);
        let found = signed_in(
            KEY,
            &token,
            &accounts(&[("alice", "$6$alice")]),
            &BTreeMap::new(),
            1000,
        );
        assert_eq!(found, None);
    }

    #[test]
    fn an_empty_key_never_accepts_a_session() {
        let token = issue("", "alice", "$6$alice", "id1", 2000);
        assert_eq!(parse("", &token), None);
    }

    #[test]
    fn a_session_whose_user_was_changed_is_refused() {
        let token = issue(KEY, "alice", "$6$alice", "id1", 2000).replacen("alice", "admin", 1);
        let found = signed_in(
            KEY,
            &token,
            &accounts(&[("alice", "$6$alice"), ("admin", "$6$alice")]),
            &BTreeMap::new(),
            1000,
        );
        assert_eq!(found, None);
    }

    #[test]
    fn an_expired_session_is_refused() {
        let token = issue(KEY, "alice", "$6$alice", "id1", 1000);
        let found = signed_in(
            KEY,
            &token,
            &accounts(&[("alice", "$6$alice")]),
            &BTreeMap::new(),
            1000,
        );
        assert_eq!(found, None);
    }

    #[test]
    fn changing_the_password_ends_the_sessions_signed_before() {
        let token = issue(KEY, "alice", "$6$old", "id1", 2000);
        let found = signed_in(
            KEY,
            &token,
            &accounts(&[("alice", "$6$new")]),
            &BTreeMap::new(),
            1000,
        );
        assert_eq!(found, None);
    }

    #[test]
    fn removing_the_account_ends_its_sessions() {
        let token = issue(KEY, "alice", "$6$alice", "id1", 2000);
        let found = signed_in(
            KEY,
            &token,
            &accounts(&[("bob", "$6$bob")]),
            &BTreeMap::new(),
            1000,
        );
        assert_eq!(found, None);
    }

    #[test]
    fn a_signed_out_session_is_refused() {
        let token = issue(KEY, "alice", "$6$alice", "id1", 2000);
        let revoked = BTreeMap::from([("id1".to_string(), 2000)]);
        let found = signed_in(
            KEY,
            &token,
            &accounts(&[("alice", "$6$alice")]),
            &revoked,
            1000,
        );
        assert_eq!(found, None);
    }

    #[test]
    fn a_token_with_an_extra_part_is_refused() {
        let token = issue(KEY, "alice", "$6$alice", "id1", 2000);
        let (body, _) = token.rsplit_once('.').unwrap();
        let forged_body = format!("{body}.extra");
        let mac = hex::encode(hmac_sha256(KEY.as_bytes(), forged_body.as_bytes()));
        assert_eq!(parse(KEY, &format!("{forged_body}.{mac}")), None);
    }
}
