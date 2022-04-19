//! Stores some of the Hola code to make conditional compilation cleaner. I should probably
//! move more code into this file.

use anyhow::Result;
use rand::prelude::SliceRandom;
use reqwest::{ClientBuilder, Proxy};
use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

use hello::ProxyType;

use crate::{common, hello, Opts};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct Config {
    uuid: Option<Uuid>,
}

/// Connect to Hola, retrieve tunnels, set the ClientBuilder to use one of the proxies. Updates
/// stored UUID in the config if we regenerated our creds.
pub(crate) async fn setup_hola(
    config: &mut Config,
    opts: &Opts,
    cb: ClientBuilder,
) -> Result<ClientBuilder> {
    let uuid = if !opts.regen_creds { config.uuid } else { None };
    let (bg, uuid) = hello::background_init(uuid).await?;
    config.uuid = Some(uuid);
    if bg.blocked || bg.permanent {
        panic!("Blocked by Hola: {:?}", bg);
    }
    let proxy_type = ProxyType::Direct;
    let tunnels = hello::get_tunnels(&uuid, bg.key, &opts.country, proxy_type, 3).await?;
    debug!("{:?}", tunnels);
    let login = hello::uuid_to_login(&uuid);
    let password = tunnels.agent_key;
    debug!("login: {}", login);
    debug!("password: {}", password);
    let (hostname, ip) =
        tunnels.ip_list.choose(&mut common::get_rng()).expect("no tunnels found in hola response");
    let port = proxy_type.get_port(&tunnels.port);
    let proxy = if !hostname.is_empty() {
        format!("https://{}:{}", hostname, port)
    } else {
        format!("http://{}:{}", ip, port)
    }; // does this check actually need to exist?
    Ok(cb.proxy(Proxy::all(proxy)?.basic_auth(&login, &password)))
}
