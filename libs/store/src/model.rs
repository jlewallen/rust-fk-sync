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
    pub removed: bool,
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
    pub removed: bool,
}

#[derive(Clone, Debug)]
pub struct LiveValue {
    pub value: f32,
    pub uncalibrated: f32,
}

#[cfg(test)]
pub(crate) mod test {
    use std::sync::atomic::AtomicI64;

    use super::*;

    #[derive(Default)]
    pub struct Build {
        module_counter: AtomicI64,
    }

    impl Build {
        pub fn station(&self) -> Station {
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

        pub fn module(&self, station_id: Option<i64>) -> Module {
            let i = self
                .module_counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Module {
                id: None,
                station_id,
                hardware_id: format!("module-{}-hardware-id", i),
                header: ModuleHeader {
                    manufacturer: 1,
                    kind: 2,
                    version: 3,
                },
                flags: 0,
                position: 0,
                name: format!("module-{}", i),
                path: format!("module-{}", i),
                configuration: None,
                removed: false,
                sensors: vec![test_sensor(None)],
            }
        }

        pub fn sensor(&self, module_id: Option<i64>) -> Sensor {
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
                removed: false,
            }
        }
    }

    pub fn test_station() -> Station {
        Build::default().station()
    }

    pub fn test_module(station_id: Option<i64>) -> Module {
        Build::default().module(station_id)
    }

    pub fn test_sensor(module_id: Option<i64>) -> Sensor {
        Build::default().sensor(module_id)
    }
}
