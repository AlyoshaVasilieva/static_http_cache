//! Traits describing parts of the `reqwest` library, so that we can override
//! them in tests.
//!
//! You do not need to care about this module
//! if you just want to use this crate.
use std::fmt;
use std::io;

use reqwest::blocking::Request;
use reqwest::StatusCode;

use crate::error::Error;

/// Represents the result of sending an HTTP request.
///
/// Modelled after `reqwest::Response`.
pub trait HttpResponse: io::Read + fmt::Debug
where
    Self: Sized,
{
    /// Obtain access to the headers of the response.
    fn headers(&self) -> &reqwest::header::HeaderMap;

    /// Obtain a copy of the response's status.
    fn status(&self) -> StatusCode;

    /// Return an error if the response's status is in the range 400-599.
    fn error_for_status(self) -> Result<Self, Error>;
}

impl HttpResponse for reqwest::blocking::Response {
    fn headers(&self) -> &reqwest::header::HeaderMap {
        self.headers()
    }
    fn status(&self) -> StatusCode {
        self.status()
    }
    fn error_for_status(self) -> Result<Self, Error> {
        Ok(self.error_for_status()?)
    }
}

/// Represents a thing that can send requests.
///
/// Modelled after `reqwest::Client`.
pub trait Client {
    /// Sending a request produces this kind of response.
    type Response: HttpResponse;

    /// Send the given request and return the response (or an error).
    fn execute(&self, request: Request) -> Result<Self::Response, Error>;
}

impl Client for reqwest::blocking::Client {
    type Response = reqwest::blocking::Response;

    fn execute(&self, request: Request) -> Result<Self::Response, Error> {
        Ok(self.execute(request)?)
    }
}

#[cfg(test)]
pub mod tests {
    use std::cell;
    use std::fmt;
    use std::io;
    use std::io::Read;

    use super::*;

    #[derive(Debug, Eq, PartialEq, Hash)]
    pub struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
            f.write_str("FakeError")?;
            Ok(())
        }
    }

    impl std::error::Error for FakeError {
        fn description(&self) -> &str {
            "Something Ooo occurred"
        }
        fn cause(&self) -> Option<&dyn std::error::Error> {
            None
        }
    }

    #[derive(Clone, Debug)]
    pub struct FakeResponse {
        pub status: StatusCode,
        pub headers: reqwest::header::HeaderMap,
        pub body: io::Cursor<Vec<u8>>,
    }

    impl super::HttpResponse for FakeResponse {
        fn headers(&self) -> &reqwest::header::HeaderMap {
            &self.headers
        }
        fn status(&self) -> StatusCode {
            self.status
        }
        fn error_for_status(self) -> Result<Self, Error> {
            if !self.status.is_client_error() && !self.status.is_server_error()
            {
                Ok(self)
            } else {
                Err(FakeError.into())
            }
        }
    }

    impl Read for FakeResponse {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.body.read(buf)
        }
    }

    pub struct FakeClient {
        pub expected_url: reqwest::Url,
        pub expected_headers: reqwest::header::HeaderMap,
        pub response: FakeResponse,
        called: cell::Cell<bool>,
    }

    impl FakeClient {
        pub fn new(
            expected_url: reqwest::Url,
            expected_headers: reqwest::header::HeaderMap,
            response: FakeResponse,
        ) -> FakeClient {
            let called = cell::Cell::new(false);
            FakeClient {
                expected_url,
                expected_headers,
                response,
                called,
            }
        }

        pub fn assert_called(self) {
            assert!(self.called.get());
        }
    }

    impl super::Client for FakeClient {
        type Response = FakeResponse;

        fn execute(&self, request: Request) -> Result<Self::Response, Error> {
            assert_eq!(request.method(), &reqwest::Method::GET);
            assert_eq!(request.url(), &self.expected_url);
            assert_eq!(request.headers(), &self.expected_headers);

            self.called.set(true);

            Ok(self.response.clone())
        }
    }

    pub struct BrokenClient<F>
    where
        F: Fn() -> Error,
    {
        pub expected_url: reqwest::Url,
        pub expected_headers: reqwest::header::HeaderMap,
        pub make_error: F,
        called: cell::Cell<bool>,
    }

    impl<F> BrokenClient<F>
    where
        F: Fn() -> Error,
    {
        pub fn new(
            expected_url: reqwest::Url,
            expected_headers: reqwest::header::HeaderMap,
            make_error: F,
        ) -> BrokenClient<F> {
            let called = cell::Cell::new(false);
            BrokenClient {
                expected_url,
                expected_headers,
                make_error,
                called,
            }
        }

        pub fn assert_called(self) {
            assert!(self.called.get());
        }
    }

    impl<F> super::Client for BrokenClient<F>
    where
        F: Fn() -> Error,
    {
        type Response = FakeResponse;

        fn execute(&self, request: Request) -> Result<Self::Response, Error> {
            assert_eq!(request.method(), &reqwest::Method::GET);
            assert_eq!(request.url(), &self.expected_url);
            assert_eq!(request.headers(), &self.expected_headers);

            self.called.set(true);

            Err((self.make_error)())
        }
    }
}
