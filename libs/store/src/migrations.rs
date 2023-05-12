use rusqlite_migration::{Migrations, M};

pub(crate) fn get_migrations<'m>() -> Migrations<'m> {
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
}
