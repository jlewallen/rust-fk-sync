use anyhow::Result;
use rusqlite::Connection;
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
}

fn get_migrations<'m>() -> Migrations<'m> {
    Migrations::new(vec![M::up(
        r#"
        CREATE TABLE station (name TEXT NOT NULL);
        "#,
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opening_in_memory_db() {
        let mut db = Db::new();
        assert!(db.open().is_ok());
    }

    #[test]
    fn test_migrations_validity() {
        assert!(get_migrations().validate().is_ok());
    }
}
