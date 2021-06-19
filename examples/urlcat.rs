extern crate env_logger;
extern crate reqwest;
extern crate static_http_cache;

use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path;

fn parse_args<T: Iterator<Item = String>>(
    mut args: T,
) -> Result<fs::File, Box<dyn Error>> {
    let cache_path = args
        .next()
        .map(|x| Ok(path::PathBuf::from(x)))
        .unwrap_or(Err("Cache directory argument required"))?;

    // Create the directory to hold persistent cache data.
    fs::DirBuilder::new().recursive(true).create(&cache_path)?;

    let raw_url = args
        .next()
        .map(Ok)
        .unwrap_or(Err("URL argument required"))?;
    let url = reqwest::Url::parse(&raw_url)?;

    let mut cache = static_http_cache::Cache::new(
        cache_path,
        reqwest::blocking::Client::new(),
    )?;

    Ok(cache.get(url)?)
}

fn main() {
    env_logger::init();

    match parse_args(env::args().skip(1)) {
        Ok(mut file) => {
            let stdout = io::stdout();
            io::copy(&mut file, &mut stdout.lock())
                .expect("could not write to stdout");
        }
        Err(e) => {
            eprintln!("Could not download URL: {:#?}", e);
        }
    }
}
