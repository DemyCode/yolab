use std::sync::OnceLock;
use std::time::Duration;

pub use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

pub fn client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .timeout(DEFAULT_TIMEOUT)
                .build()
                .unwrap_or_default()
        })
        .clone()
}

pub fn client_without_redirects() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(DEFAULT_TIMEOUT)
                .build()
                .unwrap_or_default()
        })
        .clone()
}

pub fn peer_url(addr: &str, port: u16, path: &str) -> String {
    format!("http://[{addr}]:{port}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_is_addressed_by_its_bracketed_ipv6_literal() {
        assert_eq!(
            peer_url("fd00:cafe::6", 3001, "/api/status"),
            "http://[fd00:cafe::6]:3001/api/status"
        );
    }

    #[test]
    fn a_peer_url_parses_back_to_the_same_host_and_port() {
        let url = Url::parse(&peer_url("fd00::1", 8080, "/x")).unwrap();
        assert_eq!(url.host_str(), Some("[fd00::1]"));
        assert_eq!(url.port(), Some(8080));
    }
}
