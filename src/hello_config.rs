//! Stores some of the Hola code to make conditional compilation cleaner. I should probably
//! move more code into this file.

use anyhow::{bail, Context, Result};
use rand::prelude::SliceRandom;
use reqwest::Proxy;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use uuid::Uuid;

use hello::ProxyType;

use crate::{common, hello, hello::BgInitResponse, Opts};

const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct Config {
    uuid: Option<Uuid>,
}

/// Connect to Hola, retrieve tunnels, return a Proxy. Updates
/// stored UUID in the config if we regenerated our creds.
pub(crate) async fn setup_hola(opts: &Opts) -> Result<Proxy> {
    info!(
        "Setting up Hola proxy. Regen: {} / Discard: {} / Country: {}",
        opts.regen_creds, opts.discard_creds, opts.country
    );
    let mut config: Config = confy::load(CRATE_NAME, None)?;
    let uuid = if !opts.regen_creds { config.uuid } else { None };
    let (bg, uuid) = hello::background_init(uuid).await.context("Hola init")?;
    config.uuid = Some(uuid);
    let key = match bg {
        BgInitResponse::Success { key, .. } => key,
        BgInitResponse::Block { .. } => {
            bail!("You've been blocked by Hola. Try re-running with --regen-creds. ({bg:?})");
        }
    };
    let proxy_type = ProxyType::Direct;
    let tunnels = hello::get_tunnels(&uuid, key, &opts.country, proxy_type, 3).await?;
    debug!("{:?}", tunnels);
    let login = hello::uuid_to_login(&uuid);
    let password = tunnels.agent_key;
    debug!("login: {}", login);
    debug!("password: {}", password);
    let (hostname, ip) =
        tunnels.ip_list.choose(&mut common::get_rng()).expect("no tunnels found in hola response");
    let port = proxy_type.get_port(&tunnels.port);
    let proxy = if !hostname.is_empty() {
        format!("https://{hostname}:{port}")
    } else {
        format!("http://{ip}:{port}")
    }; // does this check actually need to exist?
    if !opts.discard_creds {
        debug!(
            "Saving Hola credentials to {}",
            confy::get_configuration_file_path(CRATE_NAME, None)?.display()
        );
        confy::store(CRATE_NAME, None, &config)?;
    }
    Ok(Proxy::all(proxy)?.basic_auth(&login, &password))
}
