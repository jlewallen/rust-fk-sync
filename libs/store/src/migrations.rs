use rusqlite_migration::{Migrations, M};

pub(crate) fn get_migrations<'m>() -> Migrations<'m> {
    Migrations::new(vec![M::up(
        r#"
        CREATE TABLE station (
            id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
            device_id TEXT NOT NULL,
            generation_id TEXT NOT NULL,
            name TEXT NOT NULL,
            last_seen DATETIME NOT NULL,
            status BLOB
        );

        CREATE UNIQUE INDEX station_idx_device_id ON station (device_id);

        CREATE TABLE module (
            id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
            station_id INTEGER NOT NULL REFERENCES station(id),
            hardware_id TEXT NOT NULL,
            manufacturer INTEGER NOT NULL,
            kind INTEGER NOT NULL,
            version INTEGER NOT NULL,
            flags INTEGER NOT NULL,
            position INTEGER NOT NULL,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            configuration BLOB
        );

        CREATE INDEX module_idx_station_id ON module (station_id);

        CREATE TABLE sensor (
            id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
            module_id INTEGER NOT NULL REFERENCES module(id),
            number INTEGER NOT NULL,
            flags INTEGER NOT NULL,
            key TEXT NOT NULL,
            path TEXT NOT NULL,
            calibrated_uom TEXT NOT NULL,
            uncalibrated_uom TEXT NOT NULL,
            calibrated_value REAL,
            uncalibrated_value REAL
        );

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
}
