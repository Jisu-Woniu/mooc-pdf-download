use std::sync::RwLock;

use bytes::Bytes;
use cookie::{Cookie, ParseError};
use cookie_store::CookieStore as CookieStoreImpl;
use reqwest::{cookie::CookieStore, header::HeaderValue, Url};

#[derive(Debug, Default)]
pub struct CookieJar(RwLock<CookieStoreImpl>);

impl CookieStore for CookieJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let cookies = cookie_headers
            .filter_map(|value| {
                std::str::from_utf8(value.as_bytes())
                    .map_err(ParseError::from)
                    .and_then(Cookie::parse)
                    .ok()
            })
            .map(|c| c.into_owned());

        self.0.write().unwrap().store_response_cookies(cookies, url);
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let s = self
            .0
            .read()
            .unwrap()
            .get_request_values(url)
            .map(|(n, v)| format!("{}={}", n, v))
            .collect::<Vec<String>>()
            .join(";");

        if s.is_empty() {
            None
        } else {
            HeaderValue::from_maybe_shared(Bytes::from(s)).ok()
        }
    }
}

impl CookieJar {
    /// Add cookies to this jar.
    ///
    /// Accepts a string of cookies as a `Cookie` header value.
    pub fn add_cookie_str(&self, cookie: &str, url: &Url) {
        let cookies = cookie::Cookie::split_parse(cookie)
            .filter_map(|c| c.ok())
            .map(|c| c.into_owned());
        self.0.write().unwrap().store_response_cookies(cookies, url);
    }

    pub fn get_session_id(&self, domain: &Url) -> Option<String> {
        let cookies = self.cookies(domain).unwrap();

        let session_id = Cookie::split_parse(cookies.to_str().unwrap()).find_map(|c| {
            c.ok().and_then(|c| {
                if let ("NTESSTUDYSI", value) = c.name_value() {
                    Some(value.to_string())
                } else {
                    None
                }
            })
        });

        session_id
    }
}
