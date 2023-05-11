#[allow(unused_imports)]
use anyhow::Result;
#[allow(unused_imports)]
use prost::Message;
#[allow(unused_imports)]
use std::io::Cursor;

pub struct Client {}

impl Client {
    pub fn new() -> Self {
        Self {}
    }
}

pub mod http {
    include!(concat!(env!("OUT_DIR"), "/fk_app.rs"));
}

#[cfg(test)]
mod tests {
    use super::http::*;
    use super::*;

    #[test]
    pub fn test_parse_status() -> Result<()> {
        let data = include_bytes!("../examples/status_1.fkpb");
        let mut cursor = Cursor::new(data);
        let data = HttpReply::decode_length_delimited(&mut cursor)?;
        let status = data.status.unwrap();
        assert_eq!(status.identity.unwrap().device, "Early Impala 91");
        Ok(())
    }

    #[test]
    pub fn test_parse_status_with_logs() -> Result<()> {
        let data = include_bytes!("../examples/status_2_logs.fkpb");
        let mut cursor = Cursor::new(data);
        let data = HttpReply::decode_length_delimited(&mut cursor)?;
        let status = data.status.unwrap();
        assert_eq!(status.identity.unwrap().device, "Early Impala 91");
        assert_eq!(status.logs.len(), 32266);
        Ok(())
    }

    #[test]
    pub fn test_parse_status_with_readings() -> Result<()> {
        let data = include_bytes!("../examples/status_3_readings.fkpb");
        let mut cursor = Cursor::new(data);
        let data = HttpReply::decode_length_delimited(&mut cursor)?;
        let status = data.status.unwrap();
        assert_eq!(status.identity.unwrap().device, "Early Impala 91");
        let modules = &data.live_readings[0].modules;
        assert_eq!(modules.len(), 3);
        Ok(())
    }
}
