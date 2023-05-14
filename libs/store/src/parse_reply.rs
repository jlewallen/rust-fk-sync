use anyhow::Result;
use chrono::Utc;

use crate::model::*;
use query::HttpReply;

pub async fn http_reply_to_station(reply: HttpReply) -> Result<Station> {
    let status = reply.status.expect("No status");
    let identity = status.identity.expect("No identity");

    let device_id = DeviceId(hex::encode(identity.device_id));
    let generation_id = hex::encode(identity.generation_id);

    Ok(Station {
        id: None,
        device_id,
        name: identity.name.to_owned(),
        generation_id,
        last_seen: Utc::now(),
        modules: Vec::new(),
        status: None,
    })
}
