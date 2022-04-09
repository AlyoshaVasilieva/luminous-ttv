//! Originally based on https://github.com/Snawoot/hola-proxy
//!
//! This file is MIT licensed.
//!
//! MIT License
//!
//! Copyright (c) 2020 Snawoot
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{anyhow, Result};
use const_format::concatcp;
use isocountry::CountryCode;
use once_cell::sync::Lazy;
use rand::Rng;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
#[allow(unused)]
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::common;

// both Firefox and Chrome extensions have been removed from respective sites
// Opera version still exists: https://addons.opera.com/en/extensions/details/hola-better-internet/
// firefox version was removed later, so pretend to be FF
const EXT_VER: &str = "1.186.727";
const EXT_BROWSER: (&str, &str) = ("browser", "firefox"); // or chrome
const PRODUCT: (&str, &str) = ("product", "www"); // "cws" for Chrome Web Store
const CCGI_URL: &str = "https://client.hola.org/client_cgi/";
const BG_INIT_URL: &str = concatcp!(CCGI_URL, "background_init");
const ZGETTUNNELS_URL: &str = concatcp!(CCGI_URL, "zgettunnels");

#[allow(dead_code)] // silence clippy; this is logged
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct BgInitResponse {
    pub(crate) ver: String,
    pub(crate) key: i64,
    pub(crate) country: String,
    #[serde(default)]
    pub(crate) blocked: bool,
    #[serde(default)]
    pub(crate) permanent: bool,
}

static CLIENT: Lazy<Client> =
    Lazy::new(|| ClientBuilder::new().user_agent(crate::common::USER_AGENT).build().unwrap());

const VPN_COUNTRIES_URL: &str = concatcp!(CCGI_URL, "vpn_countries.json");

pub(crate) async fn list_countries() -> Result<()> {
    // This prints to console, which isn't really proper API design, but whatever
    let countries: Vec<String> = CLIENT
        .get(VPN_COUNTRIES_URL)
        .header(EXT_BROWSER.0, EXT_BROWSER.1)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for code in countries {
        // Hola incorrectly specifies the UK as "UK", but its alpha-2 code is GB
        // fixup the display without changing the code, since we pass that to the API
        let country = if code.eq_ignore_ascii_case("uk") {
            CountryCode::GBR
        } else {
            CountryCode::for_alpha2_caseless(&code)?
        };
        println!("{}: {}", code, country);
    }
    Ok(())
}

pub(crate) fn uuid_to_login(uuid: &Uuid) -> String {
    format!("user-uuid-{:x}", uuid.to_simple_ref()) // lowercase hex, no hyphens
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum ProxyType {
    Direct,
    /// P2P proxy. Doesn't seem to work, so this isn't actually used.
    Lum,
    // Peer,
}

impl FromStr for ProxyType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "direct" => Ok(Self::Direct),
            "lum" => Ok(Self::Lum),
            // "peer" => Ok(Self::Pool),
            other => Err(anyhow!("invalid proxy type {}", other)),
        }
    }
}

impl ProxyType {
    pub(crate) fn get_port(self, pm: &PortMap) -> u16 {
        match self {
            Self::Direct => pm.direct,
            Self::Lum => pm.hola,
            // Self::Peer => pm.peer,
        }
    }

    fn to_param(self, country: &str) -> String {
        match self {
            ProxyType::Direct => country.into(),
            ProxyType::Lum => format!("{0}.pool_lum_{0}_shared", country.to_ascii_lowercase()),
            // ProxyType::Peer => country.into(),
        }
    }
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PortMap {
    pub(crate) direct: u16,
    pub(crate) hola: u16,
    pub(crate) peer: u16,
    pub(crate) trial: u16,
    pub(crate) trial_peer: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TunnelResponse {
    pub(crate) agent_key: String,
    pub(crate) agent_types: HashMap<String, String>,
    #[serde(with = "tuple_vec_map")]
    pub(crate) ip_list: Vec<(String, String)>,
    pub(crate) port: PortMap,
    pub(crate) protocol: HashMap<String, String>,
    pub(crate) vendor: HashMap<String, String>,
    pub(crate) ztun: HashMap<String, Vec<String>>,
}

pub(crate) async fn get_tunnels(
    uuid: &Uuid,
    session_key: i64,
    country: &str,
    proxy_type: ProxyType,
    limit: u32,
) -> Result<TunnelResponse> {
    let mut rng = common::get_rng();
    let mut url = Url::parse(ZGETTUNNELS_URL).expect("zgettunnels");
    url.query_pairs_mut()
        .append_pair("country", &proxy_type.to_param(country))
        .append_pair("limit", &limit.to_string())
        .append_pair("ping_id", &rng.gen::<f64>().to_string())
        .append_pair("ext_ver", EXT_VER)
        .append_pair(EXT_BROWSER.0, EXT_BROWSER.1)
        .append_pair(PRODUCT.0, PRODUCT.1)
        .append_pair("uuid", &uuid.to_simple_ref().to_string())
        .append_pair("session_key", &session_key.to_string())
        .append_pair("is_premium", "0");
    Ok(CLIENT.get(url.as_str()).send().await?.error_for_status()?.json().await?)
}

/// Login to Hola. Generates a random UUID unless one is provided.
pub(crate) async fn background_init(uuid: Option<Uuid>) -> Result<(BgInitResponse, Uuid)> {
    debug!("bg_init using UUID {:?}", uuid);
    let uuid = uuid.unwrap_or_else(Uuid::new_v4);
    let mut url = Url::parse(BG_INIT_URL)?;
    url.query_pairs_mut().append_pair("uuid", &uuid.to_simple_ref().to_string());
    let login = &[("login", "1"), ("ver", EXT_VER)];
    let resp =
        CLIENT.post(url.as_str()).form(login).send().await?.error_for_status()?.json().await?;
    debug!("bg init response: {:?}", resp);
    Ok((resp, uuid))
}
