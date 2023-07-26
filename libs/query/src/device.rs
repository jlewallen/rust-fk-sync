use anyhow::Result;
use nonzero_ext::nonzero;
use prost::Message;
use reqwest::header::HeaderMap;
use reqwest::RequestBuilder;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use std::{io::Cursor, time::SystemTime};
use thiserror::Error;
use tokio::fs::File;
use tokio_stream::{Stream, StreamExt};
use tokio_util::io::ReaderStream;
use tracing::*;

pub use protos::http::*;

use crate::BytesUploaded;

pub struct Client {
    client: reqwest::Client,
}

#[derive(Debug, Error)]
pub enum UpgradeError {
    #[error("Server error")]
    ServerError,
}

#[derive(Debug, Default)]
pub struct ConfigureWifiTransmission {
    pub enabled: bool,
    pub token: Option<String>,
    pub url: Option<String>,
}

fn unix_time() -> Option<u64> {
    let start = SystemTime::now();
    start.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).ok()
}

impl Client {
    pub fn new() -> Result<Self> {
        let mut headers = HeaderMap::new();
        let sdk_version = std::env!("CARGO_PKG_VERSION");
        let user_agent = format!("rustfk ({})", sdk_version);
        headers.insert("user-agent", user_agent.parse()?);

        let client = reqwest::ClientBuilder::new()
            .user_agent("rustfk")
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(2))
            .default_headers(headers)
            .build()?;

        Ok(Self { client })
    }

    pub async fn query_status(&self, addr: &str) -> Result<RawAndDecoded<HttpReply>> {
        self.execute(self.new_request(addr)?.build()?).await
    }

    pub async fn query_readings(&self, addr: &str) -> Result<RawAndDecoded<HttpReply>> {
        let mut query = HttpQuery::default();
        query.r#type = QueryType::QueryGetReadings as i32;
        query.time = unix_time().unwrap_or(0);
        let encoded = query.encode_length_delimited_to_vec();
        let req = self.new_request(addr)?.body(encoded).build()?;
        self.execute(req).await
    }

    pub async fn configure_wifi_transmission(
        &self,
        addr: &str,
        configure: ConfigureWifiTransmission,
    ) -> Result<RawAndDecoded<HttpReply>> {
        let mut query = HttpQuery::default();
        query.r#type = QueryType::QueryConfigure as i32;
        // TODO Impl Into<Transmission> for ConfigureWifiTransmission
        query.transmission = Some(Transmission {
            wifi: Some(WifiTransmission {
                modifying: true,
                url: configure.url.unwrap_or_else(|| "".to_owned()),
                token: configure.token.unwrap_or_else(|| "".to_owned()),
                enabled: configure.enabled,
            }),
        });
        let encoded = query.encode_length_delimited_to_vec();
        let req = self.new_request(addr)?.body(encoded).build()?;
        self.execute(req).await
    }

    pub async fn clear_calibration(
        &self,
        addr: &str,
        module: usize,
    ) -> Result<RawAndDecoded<HttpReply>> {
        let mut query = ModuleHttpQuery::default();
        query.r#type = ModuleQueryType::ModuleQueryReset as i32;
        let encoded = query.encode_length_delimited_to_vec();
        let req = self
            .new_module_request(addr, module)?
            .body(encoded)
            .build()?;
        self.execute(req).await
    }

    pub async fn calibrate(
        &self,
        addr: &str,
        module: usize,
        data: &[u8],
    ) -> Result<RawAndDecoded<ModuleHttpReply>> {
        let mut query = ModuleHttpQuery::default();
        query.r#type = ModuleQueryType::ModuleQueryConfigure as i32;
        query.configuration = data.to_vec();
        let encoded = query.encode_length_delimited_to_vec();
        let req = self
            .new_module_request(addr, module)?
            .body(encoded)
            .build()?;
        self.execute(req).await
    }

    pub async fn upgrade(
        &self,
        addr: &str,
        path: &Path,
        swap: bool,
    ) -> Result<impl Stream<Item = Result<BytesUploaded, UpgradeError>>> {
        let file = File::open(path).await?;
        let md = file.metadata().await?;
        let total_bytes = md.len();

        let (sender, recv) =
            tokio::sync::mpsc::unbounded_channel::<Result<BytesUploaded, UpgradeError>>();

        use governor::{Quota, RateLimiter};
        let quota = Quota::per_second(nonzero!(5u32));
        let lim = RateLimiter::direct(quota);

        let mut uploaded = 0;
        let mut reader_stream = ReaderStream::new(file);

        tokio::spawn({
            let url = format!("http://{}/fk/v1/upload/firmware", addr);
            let url = if swap { format!("{}?swap=1", url) } else { url };

            let copying = sender.clone();
            let async_stream = async_stream::stream! {
                while let Some(chunk) = reader_stream.next().await {
                    if let Ok(chunk) = &chunk {
                        uploaded = std::cmp::min(uploaded + (chunk.len() as u64), total_bytes);
                        match copying.send(Ok(BytesUploaded { bytes_uploaded: uploaded, total_bytes })) {
                            Err(e) => warn!("{:?}", e),
                            Ok(_) => {},
                        }
                    }

                    lim.until_ready().await;

                    yield chunk;
                }
            };

            async move {
                info!(%url, "uploading {} bytes", total_bytes);

                let response = reqwest::Client::new()
                    .post(&url)
                    .header("content-length", format!("{}", total_bytes))
                    .body(reqwest::Body::wrap_stream(async_stream))
                    .send()
                    .await;

                match response {
                    Ok(response) => {
                        info!("done {:?}", response.status());
                        if response.status().is_server_error() {
                            match sender.send(Err(UpgradeError::ServerError)) {
                                Err(e) => warn!("{:?}", e),
                                Ok(_) => {}
                            }
                        }
                    }
                    Err(e) => warn!("{:?}", e),
                }
            }
        });

        Ok(tokio_stream::wrappers::UnboundedReceiverStream::new(recv))
    }

    async fn execute<T: Message + Default>(
        &self,
        req: reqwest::Request,
    ) -> Result<RawAndDecoded<T>> {
        let url = req.url().clone();

        debug!("{} querying", &url);
        let response = self.client.execute(req).await?;
        let bytes = response.bytes().await?;

        debug!("{} queried, got {} bytes", &url, bytes.len());
        let decoded = T::decode_length_delimited(bytes.clone())?;
        Ok(RawAndDecoded {
            bytes: bytes.to_vec(),
            decoded,
        })
    }

    fn new_module_request(&self, addr: &str, module: usize) -> Result<RequestBuilder> {
        let url = format!("http://{}/fk/v1/modules/{}", addr, module);
        Ok(self.client.post(&url).timeout(Duration::from_secs(5)))
    }

    fn new_request(&self, addr: &str) -> Result<RequestBuilder> {
        let url = format!("http://{}/fk/v1", addr);
        Ok(self.client.post(&url).timeout(Duration::from_secs(5)))
    }
}

pub struct RawAndDecoded<T> {
    pub bytes: Vec<u8>,
    pub decoded: T,
}

pub fn parse_http_reply(data: &[u8]) -> Result<HttpReply> {
    let mut cursor = Cursor::new(data);
    Ok(HttpReply::decode_length_delimited(&mut cursor)?)
}

#[cfg(test)]
mod tests {
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
