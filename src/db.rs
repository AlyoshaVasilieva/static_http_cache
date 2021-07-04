use std::cmp;
use std::ffi;
use std::fmt;
use std::iter;
use std::path;

use sqlite::Value;

use crate::error::Error;

const SCHEMA_SQL: &str = "
    CREATE TABLE urls (
        url TEXT NOT NULL UNIQUE,
        path TEXT NOT NULL,
        last_modified TEXT,
        etag TEXT,
        expires TEXT
    );
";

/// All the information we have about a given URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheRecord {
    /// The path to the cached response body on disk.
    pub path: String,
    /// The value of the Last-Modified header in the original response.
    pub last_modified: Option<String>,
    /// The value of the Etag header in the original response.
    pub etag: Option<String>,
    /// The value of the Expires header in the original response.
    pub expires: Option<String>,
}

/// Represents the rows returned by a query.
struct Rows<'a>(sqlite::Cursor<'a>);

impl<'a> iter::Iterator for Rows<'a> {
    type Item = Vec<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .next()
            .unwrap_or_else(|err| {
                warn!("Failed to get next row from SQLite: {}", err);
                None
            })
            .map(|values| values.to_vec())
    }
}

/// Represents an attempt to record information in the database.
#[must_use]
pub struct Transaction<'a> {
    conn: &'a sqlite::Connection,
    committed: bool,
}

impl<'a> Transaction<'a> {
    fn new(conn: &'a sqlite::Connection) -> Transaction<'a> {
        Transaction {
            conn,
            committed: false,
        }
    }

    pub fn commit(mut self) -> Result<(), Error> {
        debug!("Attempting to commit changes...");
        self.committed = true;

        self.conn.execute("COMMIT;").map_err(|err| {
            debug!("Failed to commit changes: {}", err);
            match self.conn.execute("ROLLBACK;") {
                // Rollback worked, return the original error
                Ok(_) => err,
                // Rollback failed too! Let's warn about that,
                // but return the original error.
                Err(new_err) => {
                    debug!("Failed to rollback too! {}", new_err);
                    err
                }
            }
        })?;
        debug!("Commit successful!");
        Ok(())
    }
}

impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        if self.committed {
            debug!("Changes already committed, nothing to do.")
        } else {
            debug!("Attempting to rollback changes...");
            self.conn.execute("ROLLBACK;").unwrap_or_else(|err| {
                debug!("Failed to rollback changes: {}", err)
            })
        }
    }
}

fn canonicalize_db_path(path: path::PathBuf) -> Result<path::PathBuf, Error> {
    let mem_path: ffi::OsString = ":memory:".into();

    Ok(if path == mem_path {
        // If it's the special ":memory:" path, use it as-is.
        path
    } else {
        let parent = path.parent().unwrap_or_else(|| path::Path::new("."));

        // Otherwise, canonicalize it so we can reliably compare instances.
        // The weird joining behaviour is because we require the path
        // to exist, but we don't require the filename to exist.
        parent
            .canonicalize()?
            .join(path.file_name().unwrap_or_else(|| ffi::OsStr::new("")))
    })
}

/// Represents the database that describes the contents of the cache.
pub struct CacheDB {
    path: path::PathBuf,
    conn: sqlite::Connection,
}

impl CacheDB {
    /// Create a cache database in the given file.
    pub fn new(path: path::PathBuf) -> Result<CacheDB, Error> {
        let path = canonicalize_db_path(path)?;
        debug!("Creating cache metadata in {:?}", path);
        let conn = sqlite::Connection::open(&path)?;

        // Package up the return value first, so we can use .query()
        // instead of wrangling sqlite directly.
        let res = CacheDB { path, conn };

        let rows: Vec<_> = res
            .query("SELECT COUNT(*) FROM sqlite_master;", &[])?
            .collect();
        if let Value::Integer(0) = rows[0][0] {
            debug!("No tables in the cache DB, loading schema.");
            res.conn.execute(SCHEMA_SQL)?
        }

        Ok(res)
    }

    fn query<'a, T: AsRef<str>>(
        &'a self,
        query: T,
        params: &[Value],
    ) -> sqlite::Result<Rows>
    where
        T: ::std::fmt::Debug,
    {
        debug!("Executing query: {:?} with values {:?}", query, params);

        let mut cur = self.conn.prepare(query)?.into_cursor();
        cur.bind(params)?;

        Ok(Rows(cur))
    }

    /// Return what the DB knows about a URL, if anything.
    pub fn get(&self, mut url: reqwest::Url) -> Result<CacheRecord, Error> {
        url.set_fragment(None);

        let mut rows = self.query(
            "
            SELECT path, last_modified, etag, expires
            FROM urls
            WHERE url = ?1
            ",
            &[Value::String(url.as_str().into())],
        )?;

        rows.next()
            .map_or_else(
                || Err(Error::URLNotFound(url.clone())),
                Ok,
            )
            .map(|row: Vec<Value>| -> Result<CacheRecord, Error> {
                let mut cols = row.into_iter();

                let path = match cols.next().unwrap() {
                    Value::String(s) => Ok(s),
                    other => Err(Error::WrongPathType(format!("{:?}", other))),
                }?;

                let last_modified = match cols.next().unwrap() {
                    Value::String(s) => Some(s),
                    Value::Null => None,
                    other => {
                        warn!(
                            "last_modified contained weird type: {:?}",
                            other,
                        );
                        None
                    },
                };

                let etag = match cols.next().unwrap() {
                    Value::String(s) => Some(s),
                    Value::Null => None,
                    other => {
                        warn!("etag contained weird type: {:?}", other);
                        None
                    },
                };

                let expires = match cols.next().unwrap() {
                    Value::String(s) => Some(s),
                    Value::Null => None,
                    other => {
                        warn!("expires contained weird type: {:?}", other);
                        None
                    },
                };

                debug!("Cache says URL {:?} content is at {:?}, etag {:?}, last modified at {:?}", url, path, etag, last_modified);

                Ok(CacheRecord{path, last_modified, etag, expires})
            })?
    }

    /// Record information about this information in the database.
    pub fn set(
        &mut self,
        mut url: reqwest::Url,
        record: CacheRecord,
    ) -> Result<Transaction, Error> {
        url.set_fragment(None);

        // TODO: Consider using the "pre-poop-your-pants" pattern to
        // ensure the transaction gets cleaned up even if somebody calls
        // mem::forget() on the Transaction object.

        // Start a new transaction...
        self.conn.execute("BEGIN;")?;

        // ...and immediately construct the value that will clean up
        // the transaction when necessary.
        let res = Transaction::new(&self.conn);

        let rows = self.query(
            "
            INSERT OR REPLACE INTO urls
                (url, path, last_modified, etag, expires)
            VALUES
                (?1, ?2, ?3, ?4, ?5);
            ",
            &[
                Value::String(url.as_str().into()),
                Value::String(record.path),
                record
                    .last_modified
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                record.etag.map(Value::String).unwrap_or(Value::Null),
                record.expires.map(Value::String).unwrap_or(Value::Null),
            ],
        )?;

        // Exhaust the row iterator to ensure the query is executed.
        for _ in rows {}

        Ok(res)
    }
}

impl fmt::Debug for CacheDB {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "CacheDB {{path: {:?}}}", self.path)
    }
}

impl cmp::PartialEq for CacheDB {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl cmp::Eq for CacheDB {}

#[cfg(test)]
mod tests {
    use std::path;

    use super::*;

    extern crate tempdir;

    #[test]
    fn create_fresh_db() {
        let db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        let rows: Vec<_> = db
            .query(
                "SELECT name FROM sqlite_master WHERE TYPE = ?1",
                &[Value::String("table".into())],
            )
            .unwrap()
            .collect();

        assert_eq!(rows, vec![vec![Value::String("urls".into())]]);
    }

    #[test]
    fn reopen_existing_db() {
        let root = tempdir::TempDir::new("cachedb-test").unwrap().into_path();
        let db_path = root.join("cache.db");

        let db1 = super::CacheDB::new(db_path.clone()).unwrap();
        let rows: Vec<_> = db1
            .query(
                "SELECT name FROM sqlite_master WHERE TYPE = ?1",
                &[Value::String("table".into())],
            )
            .unwrap()
            .collect();
        assert_eq!(rows, vec![vec![Value::String("urls".into())]]);

        let db2 = super::CacheDB::new(db_path).unwrap();
        let rows: Vec<_> = db2
            .query(
                "SELECT name FROM sqlite_master WHERE TYPE = ?1",
                &[Value::String("table".into())],
            )
            .unwrap()
            .collect();
        assert_eq!(rows, vec![vec![Value::String("urls".into())]]);
    }

    #[test]
    fn open_bogus_db() {
        let res =
            super::CacheDB::new(path::PathBuf::new().join("does/not/exist"));

        assert!(res.is_err());
    }

    #[test]
    fn get_from_empty_db() {
        let db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        let err = db.get("http://example.com/".parse().unwrap()).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                "URL not found in cache: {:?}",
                reqwest::Url::parse("http://example.com/").unwrap()
            )
        );
    }

    #[test]
    fn get_unknown_url() {
        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        db.set(
            "http://example.com/one".parse().unwrap(),
            super::CacheRecord {
                path: "path/to/data".into(),
                last_modified: None,
                etag: None,
                expires: None,
            },
        )
        .unwrap()
        .commit()
        .unwrap();

        let err = db
            .get("http://example.com/two".parse().unwrap())
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                "URL not found in cache: {:?}",
                reqwest::Url::parse("http://example.com/two").unwrap()
            )
        );
    }

    #[test]
    fn get_known_url() {
        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        let orig_record = super::CacheRecord {
            path: "path/to/data".into(),
            last_modified: None,
            etag: None,
            expires: None,
        };

        db.set("http://example.com/".parse().unwrap(), orig_record.clone())
            .unwrap()
            .commit()
            .unwrap();

        let new_record =
            db.get("http://example.com/".parse().unwrap()).unwrap();

        assert_eq!(new_record, orig_record);
    }

    #[test]
    fn get_known_url_with_headers() {
        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        let orig_record = super::CacheRecord {
            path: "path/to/data".into(),
            last_modified: Some("Thu, 01 Jan 1970 00:00:00 GMT".into()),
            etag: Some("some-etag".into()),
            expires: None,
        };

        db.set("http://example.com/".parse().unwrap(), orig_record.clone())
            .unwrap()
            .commit()
            .unwrap();

        let new_record =
            db.get("http://example.com/".parse().unwrap()).unwrap();

        assert_eq!(new_record, orig_record);
    }

    #[test]
    fn get_url_with_invalid_path() {
        let db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        db.conn
            .execute(
                "
            INSERT INTO urls
                ( url
                , path
                , last_modified
                , etag
                , expires
                )
            VALUES
                ( 'http://example.com/'
                , CAST('abc' AS BLOB)
                , NULL
                , NULL
                , NULL
                )
            ;
        ",
            )
            .unwrap();

        let err = db.get("http://example.com/".parse().unwrap()).unwrap_err();

        assert_eq!(
            err.to_string(),
            "path had wrong type: Binary([97, 98, 99])"
        );
    }

    #[test]
    fn get_url_with_invalid_last_modified_and_etag() {
        let db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        db.conn
            .execute(
                "
            INSERT INTO urls
                ( url
                , path
                , last_modified
                , etag
                , expires
                )
            VALUES
                ( 'http://example.com/'
                , 'path/to/data'
                , CAST('abc' AS BLOB)
                , CAST('def' AS BLOB)
                , CAST('ghi' AS BLOB)
                )
            ;
        ",
            )
            .unwrap();

        let record = db.get("http://example.com/".parse().unwrap()).unwrap();

        assert_eq!(
            record,
            super::CacheRecord {
                path: "path/to/data".into(),
                // We expect TEXT or NULL; if we get a BLOB value we
                // treat it as NULL.
                last_modified: None,
                etag: None,
                expires: None,
            }
        );
    }

    #[test]
    fn get_ignores_fragments() {
        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        let orig_record = super::CacheRecord {
            path: "path/to/data".into(),
            last_modified: None,
            etag: None,
            expires: None,
        };

        db.set("http://example.com/".parse().unwrap(), orig_record.clone())
            .unwrap()
            .commit()
            .unwrap();

        let new_record =
            db.get("http://example.com/#top".parse().unwrap()).unwrap();

        assert_eq!(new_record, orig_record);
    }

    #[test]
    fn insert_data_with_commit() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let record = super::CacheRecord {
            path: "path/to/data".into(),
            last_modified: None,
            etag: None,
            expires: None,
        };

        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        // Add data into the DB, inside a block so we can be sure all the
        //  intermediates have been dropped afterward.
        {
            let trans = db.set(url.clone(), record.clone()).unwrap();

            trans.commit().unwrap();
        }

        let rows: Vec<_> =
            db.query("SELECT * FROM urls;", &[]).unwrap().collect();
        debug!("Table content: {:?}", rows);

        // Did our data make it into the DB?
        assert_eq!(db.get(url).unwrap(), record);
    }

    #[test]
    fn insert_data_with_all_fields() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let record = super::CacheRecord {
            path: "path/to/data".into(),
            last_modified: Some("Thu, 01 Jan 1970 00:00:00 GMT".into()),
            etag: Some("some-etag".into()),
            expires: None,
        };

        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        // Add data into the DB, inside a block so we can be sure all the
        //  intermediates have been dropped afterward.
        db.set(url.clone(), record.clone())
            .unwrap()
            .commit()
            .unwrap();

        // Did our data make it into the DB?
        assert_eq!(db.get(url).unwrap(), record);
    }

    #[test]
    fn insert_data_without_commit() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();
        let record = super::CacheRecord {
            path: "path/to/data".into(),
            last_modified: None,
            etag: None,
            expires: None,
        };

        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        // Add data into the DB, inside a block so we can be sure all the
        //  intermediates have been dropped afterward.
        {
            let _ = db.set(url.clone(), record).unwrap();

            // Don't commit before the end of the block!
        }

        // Did our data make it into the DB?
        assert_eq!(
            db.get(url.clone()).unwrap_err().to_string(),
            format!("URL not found in cache: {:?}", url)
        );
    }

    #[test]
    fn overwrite_data() {
        let url: reqwest::Url = "http://example.com/".parse().unwrap();

        let record_one = super::CacheRecord {
            path: "path/to/data/one".into(),
            last_modified: None,
            etag: Some("one".into()),
            expires: None,
        };

        let record_two = super::CacheRecord {
            path: "path/to/data/two".into(),
            last_modified: None,
            etag: Some("two".into()),
            expires: None,
        };

        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        // Our example URL just returned record one.
        db.set(url.clone(), record_one.clone())
            .unwrap()
            .commit()
            .unwrap();

        // We recorded that correctly, right?
        assert_eq!(db.get(url.clone()).unwrap(), record_one);

        // Oh, the URL got updated!
        db.set(url.clone(), record_two.clone())
            .unwrap()
            .commit()
            .unwrap();

        // We recorded that correctly too, right?
        assert_eq!(db.get(url).unwrap(), record_two);
    }

    #[test]
    fn insert_data_ignores_url_fragment() {
        let record_one = super::CacheRecord {
            path: "path/to/data/one".into(),
            last_modified: None,
            etag: Some("one".into()),
            expires: None,
        };

        let record_two = super::CacheRecord {
            path: "path/to/data/two".into(),
            last_modified: None,
            etag: Some("two".into()),
            expires: None,
        };

        let mut db =
            super::CacheDB::new(path::PathBuf::new().join(":memory:")).unwrap();

        // Try to insert data with a fragment
        db.set(
            "http://example.com/#frag".parse().unwrap(),
            record_one.clone(),
        )
        .unwrap()
        .commit()
        .unwrap();

        // Try to insert different data without a fragment
        db.set("http://example.com/".parse().unwrap(), record_two.clone())
            .unwrap()
            .commit()
            .unwrap();

        // Querying with any fragment, or without a fragment, will always
        // give us the same information.
        assert_eq!(
            db.get("http://example.com/#frag".parse().unwrap()).unwrap(),
            record_two
        );
        assert_eq!(
            db.get("http://example.com/#garf".parse().unwrap()).unwrap(),
            record_two
        );
        assert_eq!(
            db.get("http://example.com/".parse().unwrap()).unwrap(),
            record_two
        );

        // If we insert data with a fragment, the new data is returned for
        // all queries.
        db.set(
            "http://example.com/#boop".parse().unwrap(),
            record_one.clone(),
        )
        .unwrap()
        .commit()
        .unwrap();

        assert_eq!(
            db.get("http://example.com/#frag".parse().unwrap()).unwrap(),
            record_one
        );
        assert_eq!(
            db.get("http://example.com/#garf".parse().unwrap()).unwrap(),
            record_one
        );
        assert_eq!(
            db.get("http://example.com/".parse().unwrap()).unwrap(),
            record_one
        );
    }

    #[test]
    fn dbs_are_equal_if_paths_are_equal() {
        let root = tempdir::TempDir::new("cachedb-test").unwrap().into_path();
        let db_path = root.join("cache.db");

        let db1 = super::CacheDB::new(db_path.clone()).unwrap();
        let db2 = super::CacheDB::new(db_path).unwrap();

        assert_eq!(db1, db2);
    }

    #[test]
    fn dbs_not_equal_if_paths_are_different() {
        let root = tempdir::TempDir::new("cachedb-test").unwrap().into_path();

        let db1 = super::CacheDB::new(root.join("cache-1.db")).unwrap();
        let db2 = super::CacheDB::new(root.join("cache-2.db")).unwrap();

        assert_ne!(db1, db2);
    }

    #[test]
    fn db_debug_prints_path() {
        let root = tempdir::TempDir::new("cachedb-test").unwrap().into_path();
        let path = root.join("cache.db");

        let db = super::CacheDB::new(path.clone()).unwrap();

        assert_eq!(
            format!("my db: {:?}", db),
            format!(
                "my db: CacheDB {{path: {:?}}}",
                path.canonicalize().unwrap()
            )
        );
    }
}
