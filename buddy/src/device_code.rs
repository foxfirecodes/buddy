use crate::{
    host::Pairing,
    local::{ApiError, canonical_url, checked, http, small_json},
    model::{Credentials, digest, hash_ok, id, key},
};
use anyhow::{Result, bail, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Issued {
    pub code: String,
    pub secret: String,
    pub expires_in: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Review {
    pub request: String,
    pub device: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Claim {
    pub account: String,
    pub token_hash: String,
}

pub fn normalize(input: &str) -> Option<String> {
    let code: String = input.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    (code.len() == 8 && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

#[derive(Clone)]
pub(crate) struct Pending {
    issued: Issued,
    url: String,
    device: String,
    ca: Option<String>,
    token: String,
    client: Client,
    pub deadline: Instant,
}

impl Pending {
    pub fn start(url: &str, pairing: Option<&Pairing>, device: String) -> Result<Self> {
        let url = canonical_url(url)?;
        if let Some(pair) = pairing {
            pair.validate()?;
            ensure!(
                pair.url == url,
                "pairing file belongs to a different server"
            );
        }
        let ca = pairing.map(|p| p.ca.clone());
        let client = http(ca.as_deref())?;
        let res = client
            .post(format!("{url}/v1/device-signins"))
            .json(&device)
            .timeout(Duration::from_secs(8))
            .send()?;
        if res.status() == 404 {
            bail!("Update the Buddy server to use device codes, or sign in with your password.");
        }
        let issued: Issued = small_json(res)?;
        ensure!(
            normalize(&issued.code).as_ref() == Some(&issued.code)
                && hash_ok(&issued.secret)
                && (1..=600).contains(&issued.expires_in),
            "invalid device code from server"
        );
        let deadline = Instant::now() + Duration::from_secs(issued.expires_in as u64);
        Ok(Self {
            issued,
            url,
            device,
            ca,
            token: format!("{}{}", id(), id()),
            client,
            deadline,
        })
    }

    pub fn display_code(&self) -> String {
        format!("{} {}", &self.issued.code[..4], &self.issued.code[4..])
    }
    fn post(&self, action: &str) -> RequestBuilder {
        self.client
            .post(format!("{}/v1/device-signins/{action}", self.url))
            .bearer_auth(&self.issued.secret)
            .timeout(Duration::from_secs(6))
    }
    pub fn poll(&self) -> Result<Option<String>> {
        let account: Option<String> = small_json(self.post("poll").send()?)?;
        ensure!(
            account.as_deref().is_none_or(key),
            "invalid account from server"
        );
        Ok(account)
    }
    pub fn cancel(&self) -> Result<()> {
        checked(self.post("cancel").send()?)?;
        Ok(())
    }
    pub fn finish(&self, account: &str) -> Result<Credentials> {
        let claim = Claim {
            account: account.to_owned(),
            token_hash: digest(self.token.as_bytes()),
        };
        let result = self
            .post("finish")
            .json(&claim)
            .send()
            .map_err(anyhow::Error::from)
            .and_then(checked);
        if let Err(error) = result {
            // A lost reply can mean the server already spent this code on our token.
            let check = self
                .client
                .get(format!("{}/v1/device", self.url))
                .bearer_auth(&self.token)
                .timeout(Duration::from_secs(6))
                .send();
            let confirmed = check.ok().and_then(|r| small_json::<String>(r).ok());
            if confirmed.as_deref() != Some(account) {
                return Err(error);
            }
        }
        Ok(Credentials {
            account: account.to_owned(),
            url: self.url.clone(),
            device: self.device.clone(),
            paired_ca: self.ca.clone(),
            token: self.token.clone(),
        })
    }
}

pub fn review(c: &Credentials, input: &str) -> Result<Review> {
    let code = normalize(input)
        .ok_or_else(|| anyhow::anyhow!("Enter the eight digits shown on the new device."))?;
    let response = http(c.paired_ca.as_deref())?
        .post(format!("{}/v1/device-signins/review", c.url))
        .bearer_auth(&c.token)
        .json(&code)
        .send()?;
    let review: Review = small_json(response).map_err(|e| {
        if e.downcast_ref::<ApiError>()
            .is_some_and(|err| err.0 == 404 && err.1 == "request failed")
        {
            anyhow::anyhow!("Update the Buddy server to approve a device code.")
        } else {
            e
        }
    })?;
    ensure!(
        review.request.len() == 32
            && review.request.bytes().all(|b| b.is_ascii_hexdigit())
            && !review.device.is_empty()
            && review.device.len() <= 120
            && !review.device.chars().any(char::is_control),
        "invalid device review"
    );
    Ok(review)
}
pub fn approve(c: &Credentials, review: &Review) -> Result<()> {
    checked(
        http(c.paired_ca.as_deref())?
            .post(format!("{}/v1/device-signins/approve", c.url))
            .bearer_auth(&c.token)
            .json(&review.request)
            .send()?,
    )?;
    Ok(())
}
