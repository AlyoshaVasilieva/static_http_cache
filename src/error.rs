use http::header::HeaderName;
use reqwest::Url;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error")]
    IO(#[from] std::io::Error),
    #[error("error stripping prefix")]
    StripPrefix(#[from] std::path::StripPrefixError),
    #[error("invalid header value")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("http error")]
    Http(#[from] reqwest::Error),
    #[error("database error")]
    Database(#[from] sqlite::Error),
    #[error("path had wrong type: {}", _0)]
    WrongPathType(String),
    #[error("URL not found in cache: {:?}", _0)]
    URLNotFound(Url),
    #[error("attempted to set cache-related header {}", _0)]
    DuplicateHeader(HeaderName),
    #[cfg_attr(test, error("fake error"))]
    #[cfg(test)]
    Fake(#[from] crate::reqwest_mock::tests::FakeError),
}
