use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

mod migrations;
mod model;
mod parse_reply;

pub use model::*;
pub use parse_reply::*;

pub struct Db {
    conn: Option<Connection>,
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

    pub fn update_station(&self, station: &Station) -> Result<()> {
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

        Ok(())
    }

    pub fn get_stations(&self) -> Result<Vec<Station>> {
        let mut stmt = self.require_opened()?.prepare(
            r#"SELECT id, device_id, generation_id, name, last_seen, status FROM station"#,
        )?;

        let stations = stmt.query_map(params![], |row| {
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
        })?;

        Ok(stations.map(|r| Ok(r?)).collect::<Result<Vec<_>>>()?)
    }

    pub fn add_module(&self, module: &Module) -> Result<Module> {
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
            id: id,
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

    pub fn update_module(&self, module: &Module) -> Result<()> {
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

        Ok(())
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
        let conn = self.require_opened()?;
        let mut stmt = conn.prepare(
            r#"
            INSERT INTO sensor
            (module_id, number, flags, key, path, calibrated_uom, uncalibrated_uom, calibrated_value, uncalibrated_value) VALUES
            (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )?;

        let affected = stmt.execute(params![
            sensor.module_id,
            sensor.number,
            sensor.flags,
            sensor.key,
            sensor.path,
            sensor.calibrated_uom,
            sensor.uncalibrated_uom,
            sensor.value.as_ref().map(|v| v.value),
            sensor.value.as_ref().map(|v| v.uncalibrated)
        ])?;

        assert_eq!(affected, 1);

        let id = Some(conn.last_insert_rowid());

        Ok(Sensor {
            id: id,
            module_id: sensor.module_id,
            number: sensor.number,
            flags: sensor.flags,
            key: sensor.key.clone(),
            path: sensor.path.clone(),
            calibrated_uom: sensor.calibrated_uom.clone(),
            uncalibrated_uom: sensor.uncalibrated_uom.clone(),
            value: sensor.value.clone(),
        })
    }

    pub fn update_sensor(&self, sensor: &Sensor) -> Result<()> {
        let conn = self.require_opened()?;
        let mut stmt = conn.prepare(
            r#"
            UPDATE sensor SET number = ?, flags = ?, key = ?, path = ?, calibrated_uom = ?, uncalibrated_uom = ?, calibrated_value = ?, uncalibrated_value = ? WHERE id = ?
            "#,
        )?;

        let affected = stmt.execute(params![
            sensor.number,
            sensor.flags,
            sensor.key,
            sensor.path,
            sensor.calibrated_uom,
            sensor.uncalibrated_uom,
            sensor.value.as_ref().map(|v| v.value),
            sensor.value.as_ref().map(|v| v.uncalibrated),
            sensor.id,
        ])?;

        assert_eq!(affected, 1);

        Ok(())
    }

    pub fn get_sensors(&self, module_id: i64) -> Result<Vec<Sensor>> {
        let mut stmt = self.require_opened()?.prepare(
            r#"SELECT id, module_id, number, flags, key, path, calibrated_uom, uncalibrated_uom, calibrated_value, uncalibrated_value
               FROM sensor WHERE module_id = ?"#,
        )?;

        let sensors = stmt.query_map(params![module_id], |row| {
            let calibrated_value: Option<f32> = row.get(8)?;
            let uncalibrated_value: Option<f32> = row.get(9)?;
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
                path: row.get(5)?,
                calibrated_uom: row.get(6)?,
                uncalibrated_uom: row.get(7)?,
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
    use crate::test::{test_module, test_sensor, test_station};

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

        let added = db.add_station(&test_station())?;
        assert_ne!(added.id, None);

        Ok(())
    }

    #[test]
    fn test_querying_all_stations() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        db.add_station(&test_station())?;

        let stations = db.get_stations()?;
        assert_eq!(stations.len(), 1);

        Ok(())
    }

    #[test]
    fn test_updating_station() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let mut added = db.add_station(&test_station())?;

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

        let station = db.add_station(&test_station())?;
        assert_ne!(station.id, None);

        let module = db.add_module(&test_module(station.id))?;
        assert_ne!(module.id, None);

        let modules = db.get_modules(station.id.expect("No station id"))?;
        assert_eq!(modules.len(), 1);

        Ok(())
    }

    #[test]
    fn test_updating_module() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let station = db.add_station(&test_station())?;
        assert_ne!(station.id, None);

        let mut added = db.add_module(&test_module(station.id))?;

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

        let station = db.add_station(&test_station())?;
        assert_ne!(station.id, None);

        let module = db.add_module(&test_module(station.id))?;
        assert_ne!(module.id, None);

        let sensor = db.add_sensor(&test_sensor(module.id))?;
        assert_ne!(sensor.id, None);

        let sensors = db.get_sensors(module.id.expect("No module id"))?;
        assert_eq!(sensors.len(), 1);

        Ok(())
    }

    #[test]
    fn test_updating_sensor() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        let station = db.add_station(&test_station())?;
        assert_ne!(station.id, None);

        let module = db.add_module(&test_module(station.id))?;
        assert_ne!(module.id, None);

        let mut sensor = db.add_sensor(&test_sensor(module.id))?;
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
}
