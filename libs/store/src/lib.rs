use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use rusqlite_migration::{Migrations, M};

pub mod model;

pub use model::*;

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

        let migrations = get_migrations();

        migrations.to_latest(&mut conn)?;

        self.conn = Some(conn);

        Ok(())
    }

    pub fn add_station(&self, station: &Station) -> Result<Station> {
        let conn = self.require_opened()?;
        let mut stmt =
            conn.prepare("INSERT INTO station (device_id, name, last_seen) VALUES (?, ?, ?)")?;

        let affected = stmt.execute(params![
            station.device_id.0,
            station.name,
            station.last_seen.to_rfc3339()
        ])?;

        assert_eq!(affected, 1);

        let id = Some(conn.last_insert_rowid());

        Ok(Station {
            id,
            device_id: station.device_id.clone(),
            name: station.name.clone(),
            last_seen: station.last_seen,
            modules: station.modules.clone(),
        })
    }

    pub fn get_stations(&self) -> Result<Vec<Station>> {
        let mut stmt = self
            .require_opened()?
            .prepare("SELECT id, device_id, name, last_seen FROM station")?;

        let stations = stmt.query_map(params![], |row| {
            let last_seen: String = row.get(3)?;
            let last_seen = DateTime::parse_from_rfc3339(&last_seen)
                .expect("Parsing last_seen")
                .with_timezone(&Utc);

            Ok(Station {
                id: row.get(0)?,
                device_id: DeviceId(row.get(1)?),
                name: row.get(2)?,
                last_seen,
                modules: Vec::new(),
            })
        })?;

        Ok(stations.map(|r| Ok(r?)).collect::<Result<Vec<_>>>()?)
    }

    pub fn require_opened(&self) -> Result<&Connection> {
        match &self.conn {
            Some(conn) => Ok(conn),
            None => Err(anyhow!("Expected open database")),
        }
    }
}

fn get_migrations<'m>() -> Migrations<'m> {
    Migrations::new(vec![M::up(
        r#"
        CREATE TABLE station (
            id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
            device_id TEXT NOT NULL,
            name TEXT NOT NULL,
            last_seen DATETIME NOT NULL,
            status BLOB
        );

        CREATE UNIQUE INDEX station_idx_device_id ON station (device_id);

        CREATE TABLE module (
            id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
            station_id INTEGER NOT NULL REFERENCES station(id),
            hardware_id TEXT NOT NULL,
            name TEXT NOT NULL
        ) STRICT;

        CREATE INDEX module_idx_station_id ON module (station_id);

        CREATE TABLE sensor (
            id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
            module_id INTEGER NOT NULL REFERENCES module(id),
            key TEXT NOT NULL,
            value REAL
        ) STRICT;

        CREATE INDEX sensor_idx_module_id ON sensor (module_id);
        "#,
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrations_validity() {
        assert!(get_migrations().validate().is_ok());
    }

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

        let added = db.add_station(&Station {
            id: None,
            device_id: DeviceId("device-id".to_owned()),
            name: "Hoppy Kangaroo".to_owned(),
            last_seen: Utc::now(),
            modules: Vec::new(),
        })?;

        assert_ne!(added.id, None);

        Ok(())
    }

    #[test]
    fn test_querying_all_stations() -> Result<()> {
        let mut db = Db::new();
        db.open()?;

        db.add_station(&Station {
            id: None,
            device_id: DeviceId("device-id".to_owned()),
            name: "Hoppy Kangaroo".to_owned(),
            last_seen: Utc::now(),
            modules: Vec::new(),
        })?;

        let stations = db.get_stations()?;

        assert_eq!(stations.len(), 1);

        Ok(())
    }
}
