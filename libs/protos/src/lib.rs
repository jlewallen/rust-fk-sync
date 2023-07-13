use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};
use tokio::{fs::File, io::AsyncReadExt};

pub mod data {
    include!("fk_data.rs");
}

pub mod http {
    include!("fk_app.rs");
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileMeta {
    pub sync_id: String,
    pub device_id: String,
    pub head: i64,
    pub tail: i64,
    pub headers: HashMap<String, String>,
}

impl FileMeta {
    pub async fn load_from_json(path: &Path) -> Result<Self> {
        let mut file = File::open(path).await?;
        let mut string = String::new();
        file.read_to_string(&mut string).await?;
        Ok(serde_json::from_str(&string)?)
    }

    pub fn load_from_json_sync(path: &Path) -> Result<Self> {
        use std::io::prelude::*;
        let mut file = std::fs::File::open(path)?;
        let mut string = String::new();
        file.read_to_string(&mut string)?;
        Ok(serde_json::from_str(&string)?)
    }
}
