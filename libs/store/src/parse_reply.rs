use anyhow::Result;
use chrono::Utc;

use crate::model::*;
use query::HttpReply;

pub fn http_reply_to_station(reply: HttpReply) -> Result<Station> {
    let status = reply.status.expect("No status");
    let identity = status.identity.expect("No identity");

    let device_id = DeviceId(hex::encode(identity.device_id));
    let generation_id = hex::encode(identity.generation_id);

    let modules = reply
        .modules
        .iter()
        .map(|mc| Ok(to_module(mc)?))
        .collect::<Result<Vec<_>>>()?;

    Ok(Station {
        id: None,
        device_id,
        generation_id,
        name: identity.name.to_owned(),
        last_seen: Utc::now(),
        status: None,
        modules,
    })
}

fn to_module(mc: &query::ModuleCapabilities) -> Result<Module> {
    let header = mc.header.as_ref().expect("No module header");

    let sensors = mc
        .sensors
        .iter()
        .map(|sc| Ok(to_sensor(sc)?))
        .collect::<Result<Vec<_>>>()?;

    let configuration = if mc.configuration.len() > 0 {
        Some(mc.configuration.clone())
    } else {
        None
    };

    Ok(Module {
        id: None,
        station_id: None,
        hardware_id: hex::encode(&mc.id),
        header: ModuleHeader {
            manufacturer: header.manufacturer,
            kind: header.kind,
            version: header.version,
        },
        flags: mc.flags,
        position: mc.position,
        name: mc.name.to_owned(),
        path: mc.path.to_owned(),
        configuration,
        sensors,
    })
}

fn to_sensor(sc: &query::SensorCapabilities) -> Result<Sensor> {
    Ok(Sensor {
        id: None,
        module_id: None,
        number: sc.number,
        flags: sc.flags,
        key: sc.name.to_owned(),
        path: sc.path.to_owned(),
        calibrated_uom: sc.unit_of_measure.to_owned(),
        uncalibrated_uom: sc.uncalibrated_unit_of_measure.to_owned(),
        value: sc.value.as_ref().map(|v| LiveValue {
            value: v.value,
            uncalibrated: v.uncalibrated,
        }),
    })
}

#[cfg(test)]
mod tests {
    use query::parse_http_reply;

    use super::*;

    #[test]
    pub fn test_parse_status() -> Result<()> {
        let reply = include_bytes!("../../query/examples/status_1.fkpb");
        let station = http_reply_to_station(parse_http_reply(reply)?)?;
        assert_eq!(station.name, "Early Impala 91");
        Ok(())
    }

    #[test]
    pub fn test_parse_status_with_logs() -> Result<()> {
        let reply = include_bytes!("../../query/examples/status_2_logs.fkpb");
        let station = http_reply_to_station(parse_http_reply(reply)?)?;
        assert_eq!(station.name, "Early Impala 91");
        Ok(())
    }

    #[test]
    pub fn test_parse_status_with_readings() -> Result<()> {
        let reply = include_bytes!("../../query/examples/status_3_readings.fkpb");
        let station = http_reply_to_station(parse_http_reply(reply)?)?;
        assert_eq!(station.name, "Early Impala 91");
        assert_eq!(station.modules.len(), 3);
        Ok(())
    }
}
