use anyhow::Result;
use chrono::Utc;
use thiserror::Error;

use crate::model::*;
use query::device::HttpReply;

#[derive(Error, Debug)]
pub enum ReplyMappingError {
    #[error("No status")]
    NoStatus,
    #[error("No identity")]
    NoIdentity,
    #[error("No module header")]
    NoModuleHeader,
    #[error("No sensor")]
    NoModule,
    #[error("No module")]
    NoSensor,
}

pub fn http_reply_to_station(reply: HttpReply) -> Result<Station, ReplyMappingError> {
    let status = reply.status.ok_or(ReplyMappingError::NoStatus)?;
    let identity = status.identity.ok_or(ReplyMappingError::NoIdentity)?;

    let device_id = DeviceId(hex::encode(identity.device_id));
    let generation_id = hex::encode(identity.generation_id);

    let modules = reply
        .live_readings
        .iter()
        .flat_map(|r| {
            r.modules
                .iter()
                .map(|m| Ok(to_module_with_live_readings(&m)?))
        })
        .collect::<Result<Vec<_>, ReplyMappingError>>()?;

    Ok(Station {
        id: None,
        device_id,
        generation_id,
        name: identity.name.to_owned(),
        firmware: identity.firmware.to_owned(),
        last_seen: Utc::now(),
        meta: Stream::default(),
        data: Stream::default(),
        battery: Battery::default(),
        solar: Solar::default(),
        status: None,
        modules,
    })
}

fn to_module_with_live_readings(
    m: &query::device::LiveModuleReadings,
) -> Result<Module, ReplyMappingError> {
    let sensors = m
        .readings
        .iter()
        .map(|sc| Ok(to_sensor_with_live_readings(sc)?))
        .collect::<Result<Vec<_>, ReplyMappingError>>()?;

    to_module(
        m.module.as_ref().ok_or(ReplyMappingError::NoModule)?,
        sensors,
    )
}

fn to_module(
    mc: &query::device::ModuleCapabilities,
    sensors: Vec<Sensor>,
) -> Result<Module, ReplyMappingError> {
    let header = mc
        .header
        .as_ref()
        .ok_or(ReplyMappingError::NoModuleHeader)?;

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
        key: mc.name.to_owned(),
        path: mc.path.to_owned(),
        configuration,
        removed: false,
        sensors,
    })
}

fn to_sensor_with_live_readings(
    s: &query::device::LiveSensorReading,
) -> Result<Sensor, ReplyMappingError> {
    let value = Some(LiveValue {
        value: s.value,
        uncalibrated: s.uncalibrated,
    });

    to_sensor(s.sensor.as_ref().ok_or(ReplyMappingError::NoSensor)?, value)
}

fn to_sensor(
    sc: &query::device::SensorCapabilities,
    value: Option<LiveValue>,
) -> Result<Sensor, ReplyMappingError> {
    Ok(Sensor {
        id: None,
        module_id: None,
        number: sc.number,
        flags: sc.flags,
        key: sc.name.to_owned(),
        calibrated_uom: sc.unit_of_measure.to_owned(),
        uncalibrated_uom: sc.uncalibrated_unit_of_measure.to_owned(),
        removed: false,
        value,
    })
}

#[cfg(test)]
mod tests {
    use query::device::parse_http_reply;

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
