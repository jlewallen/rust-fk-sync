use chrono::{DateTime, Utc};

// NOTE This is also declared in the `discovery` crate.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceId(pub String);

#[derive(Clone, Debug)]
pub struct Station {
    pub id: Option<i64>,
    pub device_id: DeviceId,
    pub generation_id: String,
    pub name: String,
    pub last_seen: DateTime<Utc>,
    pub status: Option<Vec<u8>>,
    pub modules: Vec<Module>,
}

#[derive(Clone, Debug)]
pub struct ModuleHeader {
    pub manufacturer: u32,
    pub kind: u32,
    pub version: u32,
}

#[derive(Clone, Debug)]
pub struct Module {
    pub id: Option<i64>,
    pub station_id: Option<i64>,
    pub hardware_id: String,
    pub header: ModuleHeader,
    pub flags: u32,
    pub position: u32,
    pub name: String,
    pub path: String,
    pub configuration: Option<Vec<u8>>,
    pub sensors: Vec<Sensor>,
}

#[derive(Clone, Debug)]
pub struct Sensor {
    pub id: Option<i64>,
    pub module_id: Option<i64>,
    pub number: u32,
    pub flags: u32,
    pub key: String,
    pub path: String,
    pub calibrated_uom: String,
    pub uncalibrated_uom: String,
    pub value: Option<LiveValue>,
}

#[derive(Clone, Debug)]
pub struct LiveValue {
    pub value: f32,
    pub uncalibrated: f32,
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;

    pub fn test_station() -> Station {
        Station {
            id: None,
            device_id: DeviceId("device-id".to_owned()),
            generation_id: "generation-id".to_owned(),
            name: "Hoppy Kangaroo".to_owned(),
            last_seen: Utc::now(),
            status: None,
            modules: vec![test_module(None)],
        }
    }

    pub fn test_module(station_id: Option<i64>) -> Module {
        Module {
            id: None,
            station_id,
            hardware_id: "module-0-hardware-id".to_owned(),
            header: ModuleHeader {
                manufacturer: 1,
                kind: 2,
                version: 3,
            },
            flags: 0,
            position: 0,
            name: "module-0".to_owned(),
            path: "module-0".to_owned(),
            configuration: None,
            sensors: vec![test_sensor(None)],
        }
    }

    pub fn test_sensor(module_id: Option<i64>) -> Sensor {
        Sensor {
            id: None,
            module_id,
            number: 1,
            flags: 0,
            key: "sensor-0".to_owned(),
            path: "sensor-0".to_owned(),
            calibrated_uom: "m".to_owned(),
            uncalibrated_uom: "mV".to_owned(),
            value: Some(LiveValue {
                value: 3.14159,
                uncalibrated: 1200.0,
            }),
        }
    }
}
