use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use thiserror::Error;
use tracing::*;

mod migrations;
mod model;
mod parse_reply;

pub use model::*;
pub use parse_reply::*;

pub struct Db {
    conn: Option<Connection>,
}

#[derive(Error, Debug)]
pub enum DbError {
    #[error("Completely unexpected")]
    SeriousBug,
}

impl Db {
    pub fn new() -> Self {
        Self { conn: None }
    }

    pub fn open(&mut self) -> Result<()> {
        let mut conn = Connection::open_in_memory()?;

        conn.pragma_update(None, "journal_mode", &"WAL")?;

        let migrations = migrations::get_migrations();

        migrations.to_latest(&mut conn)?;

        self.conn = Some(conn);

        Ok(())
    }

    fn synchronize_modules(
        &self,
        existing: Vec<Module>,
        incoming: Vec<Module>,
    ) -> Result<Vec<Module>> {
        let existing: HashMap<_, _> = existing
            .into_iter()
            .map(|m| (m.hardware_id.clone(), m))
            .collect();
        let incoming: HashMap<_, _> = incoming
            .into_iter()
            .map(|m| (m.hardware_id.clone(), m))
            .collect();

        let keys: HashSet<_> = existing
            .keys()
            .clone()
            .chain(incoming.keys().clone())
            .collect();

        Ok(keys
            .into_iter()
            .map(|key| (existing.get(key), incoming.get(key)))
            .map(|pair| match pair {
                (Some(existing), Some(incoming)) => Ok(Module {
                    position: incoming.position,
                    configuration: incoming.configuration.clone(),
                    name: incoming.name.clone(),
                    path: incoming.path.clone(),
                    sensors: self
                        .synchronize_sensors(existing.sensors.clone(), incoming.sensors.clone())?,
                    ..existing.clone()
                }),
                (None, Some(added)) => Ok(added.clone()),
                (Some(removed), None) => {
                    let mut module = removed.clone();
                    module.removed = true;
                    Ok(module)
                }
                (None, None) => panic!("Surprise module key?"),
            })
            .collect::<Result<Vec<_>>>()?)
    }

    fn synchronize_sensors(
        &self,
        existing: Vec<Sensor>,
        incoming: Vec<Sensor>,
    ) -> Result<Vec<Sensor>> {
        let existing: HashMap<_, _> = existing.into_iter().map(|m| (m.number, m)).collect();
        let incoming: HashMap<_, _> = incoming.into_iter().map(|m| (m.number, m)).collect();

        let keys: HashSet<_> = existing
            .keys()
            .clone()
            .chain(incoming.keys().clone())
            .collect();

        Ok(keys
            .into_iter()
            .map(|key| (existing.get(key), incoming.get(key)))
            .map(|pair| match pair {
                (Some(existing), Some(incoming)) => Ok(Sensor {
                    value: incoming.value.clone(),
                    ..existing.clone()
                }),
                (None, Some(added)) => Ok(added.clone()),
                (Some(removed), None) => {
                    let mut sensor = removed.clone();
                    sensor.removed = true;
                    Ok(sensor)
                }
                (None, None) => panic!("Surprise sensor key?"),
            })
            .collect::<Result<Vec<_>>>()?)
    }

    pub fn synchornize(&self, incoming: Station) -> Result<Station> {
        let existing = self.hydrate_station(&incoming.device_id)?;
        let saving = match existing {
            Some(existing) => Station {
                name: incoming.name,
                generation_id: incoming.generation_id,
                last_seen: Utc::now(),
                modules: self.synchronize_modules(existing.modules, incoming.modules)?,
                ..existing
            },
            None => incoming,
        };

        let saved = self.persist_station(&saving)?;

        info!("{:?} saved {:?}", &saved.device_id, &saved.id);

        Ok(saved)
    }

    pub fn synchronize_reply(
        &self,
        device_id: DeviceId,
        reply: query::HttpReply,
    ) -> Result<Station> {
        let incoming = http_reply_to_station(reply)?;
        assert_eq!(device_id, incoming.device_id);
        Ok(self.synchornize(incoming)?)
    }

    pub fn hydrate_station(&self, device_id: &DeviceId) -> Result<Option<Station>> {
        match self.get_station_by_device_id(device_id)? {
            Some(station) => Ok(Some(Station {
                modules: self
                    .get_modules(station.id.ok_or(DbError::SeriousBug)?)?
                    .into_iter()
                    .filter(|module| !module.removed)
                    .map(|module| {
                        Ok(Module {
                            sensors: self.get_sensors(module.id.ok_or(DbError::SeriousBug)?)?,
                            ..module
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                ..station
            })),
            None => Ok(None),
        }
    }

    pub fn persist_station(&self, station: &Station) -> Result<Station> {
        let mut station = match station.id {
            Some(_id) => self.update_station(station)?,
            None => self.add_station(station)?,
        };

        assert!(station.id.is_some());

        station.modules = station
            .modules
            .into_iter()
            .map(|module| Module {
                station_id: station.id,
                ..module
            })
            .map(|module| match module.id {
                Some(_id) => Ok(self.update_module(&module)?),
                None => Ok(self.add_module(&module)?),
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|module| {
                Ok(Module {
                    sensors: module
                        .sensors
                        .into_iter()
                        .map(|sensor| Sensor {
                            module_id: module.id,
                            ..sensor
                        })
                        .map(|sensor| {
                            Ok(match sensor.id {
                                Some(_id) => self.update_sensor(&sensor)?,
                                None => self.add_sensor(&sensor)?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?,
                    ..module
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(station)
    }

    pub fn add_station(&self, station: &Station) -> Result<Station> {
        let conn = self.require_opened()?;
        let mut stmt = conn.prepare(
            r#"
            INSERT INTO station
            (device_id, generation_id, name, last_seen, status) VALUES
            (?, ?, ?, ?, ?)
            "#,
        )?;

        let affected = stmt.execute(params![
            station.device_id.0,
            station.generation_id,
            station.name,
            station.last_seen.to_rfc3339(),
            station.status,
        ])?;

        assert_eq!(affected, 1);

        let id = Some(conn.last_insert_rowid());

        Ok(Station {
            id,
            device_id: station.device_id.clone(),
            generation_id: station.generation_id.clone(),
            name: station.name.clone(),
            last_seen: station.last_seen,
            modules: station.modules.clone(),
            status: station.status.clone(),
        })
    }

    pub fn update_station(&self, station: &Station) -> Result<Station> {
        let conn = self.require_opened()?;
        let mut stmt = conn.prepare(
            r#"UPDATE station SET generation_id = ?, name = ?, last_seen = ?, status = ? WHERE id = ?"#,
        )?;

        let affected = stmt.execute(params![
            station.generation_id,
            station.name,
            station.last_seen.to_rfc3339(),
            station.status,
            station.id,
        ])?;

        assert_eq!(affected, 1);

        Ok(station.clone())
    }

    fn row_to_station(&self, row: &rusqlite::Row) -> Result<Station, rusqlite::Error> {
        let last_seen: String = row.get(4)?;
        let last_seen = DateTime::parse_from_rfc3339(&last_seen)
            .expect("Parsing last_seen")
            .with_timezone(&Utc);

        Ok(Station {
            id: row.get(0)?,
            device_id: DeviceId(row.get(1)?),
            generation_id: row.get(2)?,
            name: row.get(3)?,
            last_seen,
            status: row.get(5)?,
            modules: Vec::new(),
        })
    }

    pub fn get_stations(&self) -> Result<Vec<Station>> {
        let mut stmt = self.require_opened()?.prepare(
            r#"SELECT id, device_id, generation_id, name, last_seen, status FROM station"#,
        )?;

        let stations = stmt.query_map(params![], |row| Ok(self.row_to_station(row)?))?;

        Ok(stations.map(|r| Ok(r?)).collect::<Result<Vec<_>>>()?)
    }

    pub fn get_station_by_device_id(&self, device_id: &DeviceId) -> Result<Option<Station>> {
        let mut stmt = self.require_opened()?.prepare(
            r#"SELECT id, device_id, generation_id, name, last_seen, status FROM station WHERE device_id = ?"#,
        )?;

        let stations = stmt.query_map(params![device_id.0], |row| Ok(self.row_to_station(row)?))?;
        let stations = stations.map(|r| Ok(r?)).collect::<Result<Vec<_>>>()?;
        Ok(stations.first().cloned())
    }

    pub fn add_module(&self, module: &Module) -> Result<Module> {
        assert!(module.id.is_none());
        assert!(module.station_id.is_some());

        let conn = self.require_opened()?;
        let mut stmt = conn.prepare(
            r#"
            INSERT INTO module
            (station_id, hardware_id, manufacturer, kind, version, flags, position, name, path, configuration, removed) VALUES
            (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )?;

        let affected = stmt.execute(params![
            module.station_id,
            module.hardware_id,
            module.header.manufacturer,
            module.header.kind,
            module.header.version,
            module.flags,
            module.position,
            module.name,
            module.path,
            module.configuration,
            module.removed,
        ])?;

        assert_eq!(affected, 1);

        let id = Some(conn.last_insert_rowid());

        Ok(Module {
            id,
            station_id: module.station_id,
            hardware_id: module.hardware_id.clone(),
            header: module.header.clone(),
            flags: module.flags,
            position: module.position,
            name: module.name.clone(),
            path: module.path.clone(),
            sensors: module.sensors.clone(),
            removed: module.removed,
            configuration: module.configuration.clone(),
        })
    }

    pub fn update_module(&self, module: &Module) -> Result<Module> {
        assert!(module.id.is_some());
        assert!(module.station_id.is_some());

        let conn = self.require_opened()?;
        let mut stmt = conn.prepare(
            r#"
            UPDATE module SET station_id = ?, manufacturer = ?, kind = ?, version = ?, flags = ?, position = ?, name = ?, path = ?, configuration = ?, removed = ? WHERE id = ?
            "#,
        )?;

        let affected = stmt.execute(params![
            module.station_id,
            module.header.manufacturer,
            module.header.kind,
            module.header.version,
            module.flags,
            module.position,
            module.name,
            module.path,
            module.configuration,
            module.removed,
            module.id,
        ])?;

        assert_eq!(affected, 1);

        Ok(module.clone())
    }

    pub fn get_modules(&self, station_id: i64) -> Result<Vec<Module>> {
        let mut stmt = self.require_opened()?.prepare(
            r#"SELECT id, station_id, hardware_id, manufacturer, kind, version, flags, position, name, path, configuration, removed
               FROM module WHERE station_id = ?"#,
        )?;

        let modules = stmt.query_map(params![station_id], |row| {
            Ok(Module {
                id: row.get(0)?,
                station_id: row.get(1)?,
                hardware_id: row.get(2)?,
                header: ModuleHeader {
                    manufacturer: row.get(3)?,
                    kind: row.get(4)?,
                    version: row.get(5)?,
                },
                flags: row.get(6)?,
                position: row.get(7)?,
                name: row.get(8)?,
                path: row.get(9)?,
                configuration: row.get(10)?,
                removed: row.get(11)?,
                sensors: Vec::new(),
            })
        })?;

        Ok(modules.map(|r| Ok(r?)).collect::<Result<Vec<_>>>()?)
    }

    pub fn add_sensor(&self, sensor: &Sensor) -> Result<Sensor> {
        assert!(sensor.id.is_none());
        assert!(sensor.module_id.is_some());

        let conn = self.require_opened()?;
        let mut stmt = conn.prepare(
            r#"
            INSERT INTO sensor
            (module_id, number, flags, key, calibrated_uom, uncalibrated_uom, calibrated_value, uncalibrated_value, removed) VALUES
            (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )?;

        let affected = stmt.execute(params![
            sensor.module_id,
            sensor.number,
            sensor.flags,
            sensor.key,
            sensor.calibrated_uom,
            sensor.uncalibrated_uom,
            sensor.value.as_ref().map(|v| v.value),
            sensor.value.as_ref().map(|v| v.uncalibrated),
            sensor.removed
        ])?;

        assert_eq!(affected, 1);

        let id = Some(conn.last_insert_rowid());

        Ok(Sensor {
            id,
            module_id: sensor.module_id,
            number: sensor.number,
            flags: sensor.flags,
            key: sensor.key.clone(),
            calibrated_uom: sensor.calibrated_uom.clone(),
            uncalibrated_uom: sensor.uncalibrated_uom.clone(),
            value: sensor.value.clone(),
            removed: sensor.removed,
        })
    }

    pub fn update_sensor(&self, sensor: &Sensor) -> Result<Sensor> {
        assert!(sensor.id.is_some());
        assert!(sensor.module_id.is_some());

        let conn = self.require_opened()?;
        let mut stmt = conn.prepare(
            r#"
            UPDATE sensor SET number = ?, flags = ?, key = ?, calibrated_uom = ?, uncalibrated_uom = ?, calibrated_value = ?, uncalibrated_value = ?, removed = ? WHERE id = ?
            "#,
        )?;

        let affected = stmt.execute(params![
            sensor.number,
            sensor.flags,
            sensor.key,
            sensor.calibrated_uom,
            sensor.uncalibrated_uom,
            sensor.value.as_ref().map(|v| v.value),
            sensor.value.as_ref().map(|v| v.uncalibrated),
            sensor.removed,
            sensor.id,
        ])?;

        assert_eq!(affected, 1);

        Ok(sensor.clone())
    }

    pub fn get_sensors(&self, module_id: i64) -> Result<Vec<Sensor>> {
        let mut stmt = self.require_opened()?.prepare(
            r#"SELECT id, module_id, number, flags, key, calibrated_uom, uncalibrated_uom, calibrated_value, uncalibrated_value, removed
               FROM sensor WHERE module_id = ?"#,
        )?;

        let sensors = stmt.query_map(params![module_id], |row| {
            let calibrated_value: Option<f32> = row.get(7)?;
            let uncalibrated_value: Option<f32> = row.get(8)?;
            let value = match (calibrated_value, uncalibrated_value) {
                (Some(calibrated), Some(uncalibrated)) => Some(LiveValue {
                    value: calibrated,
                    uncalibrated,
                }),
                _ => None,
            };

            Ok(Sensor {
                id: row.get(0)?,
                module_id: row.get(1)?,
                number: row.get(2)?,
                flags: row.get(3)?,
                key: row.get(4)?,
                calibrated_uom: row.get(5)?,
                uncalibrated_uom: row.get(6)?,
                removed: row.get(9)?,
                value,
            })
        })?;

        Ok(sensors.map(|r| Ok(r?)).collect::<Result<Vec<_>>>()?)
    }

    pub fn require_opened(&self) -> Result<&Connection> {
        match &self.conn {
            Some(conn) => Ok(conn),
            None => Err(anyhow!("Expected open database")),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test::*;

    use super::*;

    #[test]
    fn test_opening_in_memory_db() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        Ok(())
    }

    #[test]
    fn test_adding_new_station() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let added = db.add_station(&BuildStation::default().build())?;
        assert_ne!(added.id, None);

        Ok(())
    }

    #[test]
    fn test_querying_all_stations() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        db.add_station(&BuildStation::default().build())?;

        let stations = db.get_stations()?;
        assert_eq!(stations.len(), 1);

        Ok(())
    }

    #[test]
    fn test_updating_station() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let mut added = db.add_station(&BuildStation::default().build())?;

        let stations = db.get_stations()?;
        assert_eq!(stations.len(), 1);
        assert_eq!(stations.get(0).unwrap().name, "Hoppy Kangaroo");

        added.name = "Tired Kangaroo".to_owned();
        db.update_station(&added)?;

        let stations = db.get_stations()?;
        assert_eq!(stations.len(), 1);
        assert_eq!(stations.get(0).unwrap().name, "Tired Kangaroo");

        Ok(())
    }

    #[test]
    fn test_adding_module() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let station = db.add_station(&BuildStation::default().build())?;
        assert_ne!(station.id, None);

        let module = db.add_module(&BuildModule::default().station_id(station.id).build())?;
        assert_ne!(module.id, None);

        let modules = db.get_modules(station.id.expect("No station id"))?;
        assert_eq!(modules.len(), 1);

        Ok(())
    }

    #[test]
    fn test_updating_module() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let station = db.add_station(&BuildStation::default().build())?;
        assert_ne!(station.id, None);

        let mut added = db.add_module(
            &BuildModule::default()
                .hardware_id("module-0")
                .named("module-0")
                .station_id(station.id)
                .build(),
        )?;

        let modules = db.get_modules(station.id.expect("No station id"))?;
        assert_eq!(modules.len(), 1);
        assert_eq!(modules.get(0).unwrap().name, "module-0");

        added.name = "renamed-module-0".to_owned();
        db.update_module(&added)?;

        let modules = db.get_modules(station.id.expect("No station id"))?;
        assert_eq!(modules.len(), 1);
        assert_eq!(modules.get(0).unwrap().name, "renamed-module-0");

        Ok(())
    }

    #[test]
    fn test_adding_sensor() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let station = db.add_station(&BuildStation::default().build())?;
        assert_ne!(station.id, None);

        let module = db.add_module(&BuildModule::default().station_id(station.id).build())?;
        assert_ne!(module.id, None);

        let sensor = db.add_sensor(&BuildSensor::default().module_id(module.id).build())?;
        assert_ne!(sensor.id, None);

        let sensors = db.get_sensors(module.id.expect("No module id"))?;
        assert_eq!(sensors.len(), 1);

        Ok(())
    }

    #[test]
    fn test_updating_sensor() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let station = db.add_station(&build().station().build())?;
        assert_ne!(station.id, None);

        let module = db.add_module(&build().module().station_id(station.id).build())?;
        assert_ne!(module.id, None);

        let mut sensor = db.add_sensor(&BuildSensor::default().module_id(module.id).build())?;
        assert_ne!(sensor.id, None);

        let sensors = db.get_sensors(module.id.expect("No module id"))?;
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors.get(0).unwrap().key, "sensor-0");

        sensor.key = "renamed-sensor-0".to_owned();
        db.update_sensor(&sensor)?;

        let sensors = db.get_sensors(module.id.expect("No module id"))?;
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors.get(0).unwrap().key, "renamed-sensor-0");

        Ok(())
    }

    #[test]
    fn test_sync_new_station() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let incoming = BuildStation::default().with_basic_module("basic-0").build();
        let station = db.synchornize(incoming)?;

        assert!(station.id.is_some());

        for module in station.modules {
            assert!(module.id.is_some());
            for sensor in module.sensors {
                assert!(sensor.id.is_some());
            }
        }

        Ok(())
    }

    #[test]
    fn test_sync_station_with_only_field_changes() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let incoming = BuildStation::default().with_basic_module("basic-0").build();
        let first = db.synchornize(incoming)?;

        assert!(first.id.is_some());

        let mut incoming = BuildStation::default().with_basic_module("basic-0").build();
        incoming.name = "Renamed".to_owned();
        let second = db.synchornize(incoming)?;

        assert_eq!(first.id, second.id);
        assert_eq!(second.name, "Renamed");

        Ok(())
    }

    #[test]
    fn test_sync_station_with_module_removed() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let incoming = BuildStation::default().with_basic_module("basic-0").build();
        let first = db.synchornize(incoming)?;

        assert!(first.id.is_some());

        let incoming = BuildStation::default().build();
        let second = db.synchornize(incoming)?;

        assert_eq!(first.id, second.id);
        assert_eq!(second.modules.len(), 1);
        assert_eq!(second.modules.get(0).map(|m| m.removed), Some(true));

        Ok(())
    }

    #[test]
    fn test_sync_station_with_module_added() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let incoming = BuildStation::default().with_basic_module("basic-0").build();
        let first = db.synchornize(incoming)?;

        assert!(first.id.is_some());

        let incoming = BuildStation::default()
            .with_basic_module("basic-0")
            .with_basic_module("basic-1")
            .build();
        let second = db.synchornize(incoming)?;

        assert_eq!(first.id, second.id);
        assert_eq!(second.modules.len(), 2);

        Ok(())
    }
}
