use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};
use tokio::{fs::File, io::AsyncReadExt};

pub mod data {
    include!(concat!(env!("OUT_DIR"), "/fk_data.rs"));
}

pub mod http {
    include!(concat!(env!("OUT_DIR"), "/fk_app.rs"));
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileMeta {
    pub headers: HashMap<String, String>,
}

impl FileMeta {
    pub async fn load_from_json(path: &Path) -> Result<Self> {
        let mut file = File::open(path).await?;
        let mut string = String::new();
        file.read_to_string(&mut string).await?;
        Ok(serde_json::from_str(&string)?)
    }
}
