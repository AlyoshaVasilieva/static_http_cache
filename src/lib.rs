//! A local cache for static HTTP resources.
//!
//! Ain't that great?
//!
//! You probably want to create a `Cache` and call the `get()` method.
//!
//! TODO:
//!
//!   - break big long functions up into helper functions?
//!   - write an example tool that uses the library
//!   - proper error reporting
//!   - write documentation!
//!   - evaluate API against the API guidelines:
//!     https://rust-lang-nursery.github.io/api-guidelines/
extern crate crypto_hash;
#[macro_use]
extern crate log;
extern crate reqwest;
extern crate sqlite;
extern crate rand;


use std::error;
use std::fs;
use std::io;
use std::path;


pub mod reqwest_mock;


mod db;


fn make_random_file<P: AsRef<path::Path>>(parent: P)
    -> Result<(fs::File, path::PathBuf), Box<error::Error>>
{
    use rand::Rng;
    let mut rng = rand::thread_rng();

    loop {
        let new_path = parent
            .as_ref()
            .join(rng.gen_ascii_chars().take(20).collect::<String>());

        let maybe_handle = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&new_path);

        match maybe_handle {
            Ok(handle) => { return Ok((handle, new_path)) },
            Err(e) => {
                if e.kind() != io::ErrorKind::AlreadyExists {
                    // An actual error, we'd better report it!
                    return Err(e.into())
                }

                // Otherwise, we'll just continue around the loop and
                // try again.
            },
        };
    }
}


/// Represents a local cache of HTTP bodies.
///
/// Requests sent via this cache to URLs it hasn't seen before will
/// automatically be cached; requests sent to previously-seen URLs will be
/// revalidated against the server and returned from the cache if nothing has
/// changed.
pub struct Cache<C: reqwest_mock::Client> {
    root: path::PathBuf,
    db: db::CacheDB,
    client: C,
}


impl<C: reqwest_mock::Client> Cache<C> {

    /// Returns a Cache that wraps `client` and caches data in `root`.
    ///
    /// If the directory `root` does not exist, it will be created.
    /// If it does exist, previously cached data will be available.
    ///
    /// The client should almost certainly be a `reqwest::Client`.
    pub fn new(root: path::PathBuf, client: C)
        -> Result<Cache<C>, Box<error::Error>>
    {
        fs::DirBuilder::new()
            .recursive(true)
            .create(&root)?;

        let db = db::CacheDB::new(root.join("cache.db"))?;

        Ok(Cache { root, db, client })
    }

    pub fn get(&mut self, mut url: reqwest::Url)
        -> Result<fs::File, Box<error::Error>>
    {
        use reqwest_mock::HttpResponse;
        use reqwest::StatusCode;

        url.set_fragment(None);

        let mut response = match self.db.get(url.clone()) {
            Ok(db::CacheRecord{path: p, last_modified: lm, etag: et}) => {
                // We have a locally-cached copy, let's check whether the
                // copy on the server has changed.
                let mut request = reqwest::Request::new(
                    reqwest::Method::Get,
                    url.clone(),
                );
                if let Some(timestamp) = lm {
                    request.headers_mut().set(
                        reqwest::header::IfModifiedSince(timestamp),
                    );
                }
                if let Some(etag) = et {
                    request.headers_mut().set(
                        reqwest::header::IfNoneMatch::Items(vec![etag]),
                    );
                }

                let maybe_validation = self.client
                    .execute(request)
                    .and_then(|resp| { resp.error_for_status() });

                match maybe_validation {
                    Ok(new_response) => {
                        // If our existing cached data is still fresh...
                        if new_response.status() == StatusCode::NotModified {
                            // ... let's use it as is.
                            return Ok(fs::File::open(self.root.join(p))?);
                        }

                        // Otherwise, we got a new response we need to cache.
                        new_response
                    },
                    Err(e) => {
                        warn!("Could not validate cached response: {}", e);

                        // Let's just use the existing data we have.
                        return Ok(fs::File::open(self.root.join(p))?);
                    },
                }
            },
            Err(_) => {
                // This URL isn't in the cache, or we otherwise can't find it.
                self.client.execute(
                    reqwest::Request::new(reqwest::Method::Get, url.clone()),
                )?.error_for_status()?
            },
        };

        // Record the response from the server in our cache.
        let content_dir = self.root.join("content");
        fs::DirBuilder::new()
            .recursive(true)
            .create(&content_dir)?;

        let (mut handle, path) = make_random_file(&content_dir)?;
        let rel_path = path.strip_prefix(&self.root)?;

        let trans = self.db.set(
            url,
            db::CacheRecord {
                path: rel_path.to_str().unwrap().into(),
                last_modified: response.headers()
                    .get::<reqwest::header::LastModified>()
                    .map(|&reqwest::header::LastModified(date)| {
                        date
                    }),
                etag: response.headers()
                    .get::<reqwest::header::ETag>()
                    .map(|&reqwest::header::ETag(ref etag)| {
                        etag.clone()
                    }),
            },
        )?;

        let count = io::copy(
            &mut response,
            &mut handle,
        )?;

        debug!("Downloaded {} bytes", count);

        trans.commit()?;

        Ok(fs::File::open(&path)?)
    }
}


#[cfg(test)]
mod tests {
    extern crate tempdir;

    use reqwest;

    use std::io;

    use std::io::Read;

    use super::reqwest_mock::tests as rmt;


    fn make_test_cache(client: rmt::FakeClient)
        -> super::Cache<rmt::FakeClient>
    {
        super::Cache::new(
            tempdir::TempDir::new("http-cache-test").unwrap().into_path(),
            client,
        ).unwrap()
    }


    #[test]
    fn initial_request_success() {
        let url_text = "http://example.com/";
        let url: reqwest::Url = url_text.parse().unwrap();

        let body = b"hello world";

        let mut c = make_test_cache(
            rmt::FakeClient::new(
                url.clone(),
                reqwest::header::Headers::default(),
                rmt::FakeResponse{
                    status: reqwest::StatusCode::Ok,
                    headers: reqwest::header::Headers::default(),
                    body: io::Cursor::new(body.as_ref().into()),
                }
            ),
        );

        // We should get a file-handle containing the body bytes.
        let mut res = c.get(url).unwrap();
        let mut buf = vec![];
        res.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, body);
        c.client.assert_called();
    }

    #[test]
    fn initial_request_failure() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let mut c = make_test_cache(
            rmt::FakeClient::new(
                url.clone(),
                reqwest::header::Headers::default(),
                rmt::FakeResponse{
                    status: reqwest::StatusCode::InternalServerError,
                    headers: reqwest::header::Headers::default(),
                    body: io::Cursor::new(vec![]),
                }
            ),
        );

        let err = c.get(url).expect_err("Got a response??");
        assert_eq!(format!("{}", err), "FakeError");
        c.client.assert_called();
    }

    #[test]
    fn ignore_fragment_in_url() {
        let url_fragment: reqwest::Url = "http://example.com/#frag"
            .parse()
            .unwrap();

        let mut network_url = url_fragment.clone();
        network_url.set_fragment(None);

        let mut c = make_test_cache(
            rmt::FakeClient::new(
                // We expect the cache to request the URL without the fragment.
                network_url,
                reqwest::header::Headers::default(),
                rmt::FakeResponse{
                    status: reqwest::StatusCode::Ok,
                    headers: reqwest::header::Headers::default(),
                    body: io::Cursor::new(b"hello world"[..].into()),
                }
            ),
        );

        // Ask for the URL with the fragment.
        c.get(url_fragment).unwrap();
    }

    #[test]
    fn use_cache_data_if_not_modified_since() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let body = b"hello world";

        let now = ::std::time::SystemTime::now();

        // We send a request, and the server responds with the data,
        // and a "Last-Modified" header.
        let mut response_headers = reqwest::header::Headers::default();
        response_headers.set(reqwest::header::LastModified(now.into()));

        let mut c = make_test_cache(
            rmt::FakeClient::new(
                url.clone(),
                reqwest::header::Headers::default(),
                rmt::FakeResponse{
                    status: reqwest::StatusCode::Ok,
                    headers: response_headers.clone(),
                    body: io::Cursor::new(body.as_ref().into()),
                }
            ),
        );

        // The response and its last-modified date should now be recorded
        // in the cache.
        c.get(url.clone()).unwrap();
        c.client.assert_called();

        // For the next request, we expect the request to include the
        // modified date in the "if modified since" header, and we'll give
        // the "no, it hasn't been modified" response.
        let mut second_request = reqwest::header::Headers::default();
        second_request.set(reqwest::header::IfModifiedSince(now.into()));

        c.client = rmt::FakeClient::new(
            url.clone(),
            second_request,
            rmt::FakeResponse{
                status: reqwest::StatusCode::NotModified,
                headers: response_headers,
                body: io::Cursor::new(b""[..].into()),
            },
        );

        // Now when we make the request, even though the actual response
        // did not include a body, we should get the complete body from
        // the local cache.
        let mut res = c.get(url).unwrap();
        let mut buf = vec![];
        res.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, body);
        c.client.assert_called();
    }

    #[test]
    fn update_cache_if_modified_since() {
        use std::str::FromStr;

        let url: reqwest::Url = "http://example.com/".parse().unwrap();

        // We send a request, and the server responds with the data,
        // and a "Last-Modified" header.
        let request_1_headers = reqwest::header::Headers::default();
        let mut response_1_headers = reqwest::header::Headers::default();
        response_1_headers.set(reqwest::header::LastModified(
            reqwest::header::HttpDate::from_str(
                "Thu, 01 Jan 1970 00:00:00 GMT"
            ).unwrap(),
        ));

        let mut c = make_test_cache(
            rmt::FakeClient::new(
                url.clone(),
                request_1_headers,
                rmt::FakeResponse{
                    status: reqwest::StatusCode::Ok,
                    headers: response_1_headers,
                    body: io::Cursor::new(b"hello".as_ref().into()),
                }
            ),
        );

        // The response and its last-modified date should now be recorded
        // in the cache.
        c.get(url.clone()).unwrap();
        c.client.assert_called();

        // For the next request, we expect the request to include the
        // modified date in the "if modified since" header, and we'll give
        // the "yes, it has been modified" response with a new Last-Modified.
        let mut request_2_headers = reqwest::header::Headers::default();
        request_2_headers.set(reqwest::header::IfModifiedSince(
            reqwest::header::HttpDate::from_str(
                "Thu, 01 Jan 1970 00:00:00 GMT"
            ).unwrap(),
        ));
        let mut response_2_headers = reqwest::header::Headers::default();
        response_2_headers.set(reqwest::header::LastModified(
            reqwest::header::HttpDate::from_str(
                "Thu, 01 Jan 1970 00:01:00 GMT"
            ).unwrap(),
        ));

        c.client = rmt::FakeClient::new(
            url.clone(),
            request_2_headers,
            rmt::FakeResponse{
                status: reqwest::StatusCode::Ok,
                headers: response_2_headers,
                body: io::Cursor::new(b"world".as_ref().into()),
            },
        );

        // Now when we make the request, we should get the new body and
        // ignore what's in the cache.
        let mut res = c.get(url.clone()).unwrap();
        let mut buf = vec![];
        res.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"world");
        c.client.assert_called();

        // If we make another request, we should set If-Modified-Since
        // to match the second response, and be able to return the data from
        // the second response.
        let mut request_3_headers = reqwest::header::Headers::default();
        request_3_headers.set(reqwest::header::IfModifiedSince(
            reqwest::header::HttpDate::from_str(
                "Thu, 01 Jan 1970 00:01:00 GMT"
            ).unwrap(),
        ));
        let response_3_headers = reqwest::header::Headers::default();

        c.client = rmt::FakeClient::new(
            url.clone(),
            request_3_headers,
            rmt::FakeResponse{
                status: reqwest::StatusCode::NotModified,
                headers: response_3_headers,
                body: io::Cursor::new(b"".as_ref().into()),
            },
        );

        // Now when we make the request, we should get updated info from the
        // cache.
        let mut res = c.get(url).unwrap();
        let mut buf = vec![];
        res.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"world");
        c.client.assert_called();
    }


    #[test]
    fn return_existing_data_on_connection_refused() {
        use std::str::FromStr;

        let temp_path = tempdir::TempDir::new("http-cache-test")
            .unwrap()
            .into_path();

        let url: reqwest::Url = "http://example.com/".parse().unwrap();

        // We send a request, and the server responds with the data,
        // and a "Last-Modified" header.
        let request_1_headers = reqwest::header::Headers::default();
        let mut response_1_headers = reqwest::header::Headers::default();
        response_1_headers.set(reqwest::header::LastModified(
            reqwest::header::HttpDate::from_str(
                "Thu, 01 Jan 1970 00:00:00 GMT"
            ).unwrap(),
        ));

        let mut c = super::Cache::new(
            temp_path.clone(),
            rmt::FakeClient::new(
                url.clone(),
                request_1_headers,
                rmt::FakeResponse{
                    status: reqwest::StatusCode::Ok,
                    headers: response_1_headers,
                    body: io::Cursor::new(b"hello".as_ref().into()),
                }
            ),
        ).unwrap();

        // The response and its last-modified date should now be recorded
        // in the cache.
        c.get(url.clone()).unwrap();
        c.client.assert_called();

        // If we make second request, we should set If-Modified-Since
        // to match the first response's Last-Modified.
        let mut request_2_headers = reqwest::header::Headers::default();
        request_2_headers.set(reqwest::header::IfModifiedSince(
            reqwest::header::HttpDate::from_str(
                "Thu, 01 Jan 1970 00:00:00 GMT"
            ).unwrap(),
        ));

        // This time, however, the request will return an error.
        let mut c = super::Cache::new(
            temp_path.clone(),
            rmt::BrokenClient::new(
                url.clone(),
                request_2_headers,
                || { rmt::FakeError.into() },
            ),
        ).unwrap();

        // Now when we request a URL, we should get the cached result.
        let mut res = c.get(url.clone()).unwrap();
        let mut buf = vec![];
        res.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        c.client.assert_called();
    }

    #[test]
    fn use_cache_data_if_some_match() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let body = b"hello world";

        // We send a request, and the server responds with the data,
        // and an "Etag" header.
        let mut response_headers = reqwest::header::Headers::default();
        response_headers.set(
            reqwest::header::ETag(
                reqwest::header::EntityTag::strong("abcd".into()),
            ),
        );

        let mut c = make_test_cache(
            rmt::FakeClient::new(
                url.clone(),
                reqwest::header::Headers::default(),
                rmt::FakeResponse{
                    status: reqwest::StatusCode::Ok,
                    headers: response_headers.clone(),
                    body: io::Cursor::new(body.as_ref().into()),
                }
            ),
        );

        // The response and its etag should now be recorded
        // in the cache.
        c.get(url.clone()).unwrap();
        c.client.assert_called();

        // For the next request, we expect the request to include the
        // etag in the "if none match" header, and we'll give
        // the "no, it hasn't been modified" response.
        let mut second_request = reqwest::header::Headers::default();
        second_request.set(
            reqwest::header::IfNoneMatch::Items(
                vec![
                    reqwest::header::EntityTag::strong("abcd".into()),
                ],
            ),
        );

        c.client = rmt::FakeClient::new(
            url.clone(),
            second_request,
            rmt::FakeResponse{
                status: reqwest::StatusCode::NotModified,
                headers: response_headers,
                body: io::Cursor::new(b""[..].into()),
            },
        );

        // Now when we make the request, even though the actual response
        // did not include a body, we should get the complete body from
        // the local cache.
        let mut res = c.get(url).unwrap();
        let mut buf = vec![];
        res.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, body);
        c.client.assert_called();
    }

    #[test]
    fn update_cache_if_none_match() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();

        // We send a request, and the server responds with the data,
        // and an "ETag" header.
        let request_1_headers = reqwest::header::Headers::default();
        let mut response_1_headers = reqwest::header::Headers::default();
        response_1_headers.set(
            reqwest::header::ETag(
                reqwest::header::EntityTag::strong("abcd".into()),
            ),
        );

        let mut c = make_test_cache(
            rmt::FakeClient::new(
                url.clone(),
                request_1_headers,
                rmt::FakeResponse{
                    status: reqwest::StatusCode::Ok,
                    headers: response_1_headers,
                    body: io::Cursor::new(b"hello".as_ref().into()),
                }
            ),
        );

        // The response and its etag should now be recorded in the cache.
        c.get(url.clone()).unwrap();
        c.client.assert_called();

        // For the next request, we expect the request to include the
        // etag in the "if none match" header, and we'll give
        // the "yes, it has been modified" response with a new etag.
        let mut request_2_headers = reqwest::header::Headers::default();
        request_2_headers.set(
            reqwest::header::IfNoneMatch::Items(
                vec![
                    reqwest::header::EntityTag::strong("abcd".into()),
                ],
            ),
        );
        let mut response_2_headers = reqwest::header::Headers::default();
        response_2_headers.set(
            reqwest::header::ETag(
                reqwest::header::EntityTag::strong("efgh".into()),
            ),
        );

        c.client = rmt::FakeClient::new(
            url.clone(),
            request_2_headers,
            rmt::FakeResponse{
                status: reqwest::StatusCode::Ok,
                headers: response_2_headers,
                body: io::Cursor::new(b"world".as_ref().into()),
            },
        );

        // Now when we make the request, we should get the new body and
        // ignore what's in the cache.
        let mut res = c.get(url.clone()).unwrap();
        let mut buf = vec![];
        res.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"world");
        c.client.assert_called();

        // If we make another request, we should set If-None-Match
        // to match the second response, and be able to return the data from
        // the second response.
        let mut request_3_headers = reqwest::header::Headers::default();
        request_3_headers.set(
            reqwest::header::IfNoneMatch::Items(
                vec![
                    reqwest::header::EntityTag::strong("efgh".into()),
                ],
            ),
        );
        let response_3_headers = reqwest::header::Headers::default();

        c.client = rmt::FakeClient::new(
            url.clone(),
            request_3_headers,
            rmt::FakeResponse{
                status: reqwest::StatusCode::NotModified,
                headers: response_3_headers,
                body: io::Cursor::new(b"".as_ref().into()),
            },
        );

        // Now when we make the request, we should get updated info from the
        // cache.
        let mut res = c.get(url).unwrap();
        let mut buf = vec![];
        res.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"world");
        c.client.assert_called();
    }


    // See also: https://developer.mozilla.org/en-US/docs/Web/HTTP/Caching
}
