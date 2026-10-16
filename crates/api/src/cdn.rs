//! Downloads from Discord's CDN: profile pictures and server icons. No token is sent.

use std::time::Duration;

use crate::error::Error;

const TIMEOUT: Duration = Duration::from_secs(15);

pub struct Cdn {
    client: reqwest::Client,
}

impl Cdn {
    pub fn new() -> Result<Self, Error> {
        Ok(Self { client: reqwest::Client::builder().timeout(TIMEOUT).build()? })
    }

    pub async fn get(&self, url: &str) -> Result<Vec<u8>, Error> {
        let response = self.client.get(url).send().await?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(Error::Status(status));
        }
        Ok(response.bytes().await?.to_vec())
    }
}
