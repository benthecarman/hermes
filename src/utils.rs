use std::{fmt::Display, str::FromStr};

use crate::config::CONFIG;
use anyhow::Result;
use serde::{de, Deserialize, Deserializer};
use xmpp::Agent;

pub fn empty_string_as_none<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    let opt = Option::<String>::deserialize(de)?;
    match opt.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => FromStr::from_str(s).map_err(de::Error::custom).map(Some),
    }
}

// TODO: XMPP client doesn't implement Clone, so we can't use it in AppState which is annoying
pub async fn create_xmpp_client() -> Result<Agent> {
    let jid = xmpp::BareJid::new(&format!("{}@{}", CONFIG.xmpp_username, CONFIG.xmpp_chat_server))?;
    let client = xmpp::ClientBuilder::new(jid, &CONFIG.xmpp_password).build();

    Ok(client)
}
