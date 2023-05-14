use chrono::{DateTime, Utc};

// NOTE This is also declared in the `discovery` crate.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceId(pub String);

#[allow(dead_code)]
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

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Module {
    pub id: Option<u64>,
    pub hardware_id: String,
    pub name: String,
    pub sensors: Vec<Sensor>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Sensor {
    pub id: Option<u64>,
    pub key: String,
    pub value: Option<f32>,
}
