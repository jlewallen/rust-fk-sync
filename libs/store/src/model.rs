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
    pub modules: Vec<Module>,
    pub status: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct ModuleHeader {
    pub manufacturer: u32,
    pub kind: u32,
    pub version: u32,
}

#[derive(Clone, Debug)]
pub struct Module {
    pub id: Option<u64>,
    pub hardware_id: String,
    pub header: ModuleHeader,
    pub flags: u32,
    pub position: u32,
    pub name: String,
    pub path: String,
    pub sensors: Vec<Sensor>,
    pub configuration: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct Sensor {
    pub id: Option<u64>,
    pub number: u32,
    pub flags: u32,
    pub key: String,
    pub path: String,
    pub uom: String,
    pub uncalibrated_uom: String,
    pub value: Option<LiveValue>,
}

#[derive(Clone, Debug)]
pub struct LiveValue {
    pub value: f32,
    pub uncalibrated: f32,
}
