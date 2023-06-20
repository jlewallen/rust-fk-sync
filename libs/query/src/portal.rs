use anyhow::Result;
use base64::{engine::general_purpose, Engine};
use chrono::serde::ts_seconds::deserialize as from_ts;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use reqwest::{header::InvalidHeaderValue, header::ToStrError};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Request, Response,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use tokio::fs::File;
use tokio::sync::mpsc::Sender;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use protos::FileMeta;

pub use reqwest::StatusCode;

#[derive(Debug)]
pub struct Client {
    base_url: String,
    client: reqwest::Client,
}

impl Client {
    pub fn new(base_url: &str) -> Result<Self, PortalError> {
        Self::new_with_headers(base_url, HeaderMap::new())
    }

    pub fn new_with_headers(base_url: &str, mut headers: HeaderMap) -> Result<Self, PortalError> {
        let sdk_version = std::env!("CARGO_PKG_VERSION");
        let user_agent = format!("rustfk ({})", sdk_version);
        headers.insert("user-agent", user_agent.parse()?);

        let client = reqwest::ClientBuilder::new()
            .user_agent("rustfk")
            .timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()?;

        Ok(Self {
            base_url: base_url.to_owned(),
            client,
        })
    }

    async fn execute_req(&self, req: Request) -> Result<Response, PortalError> {
        let url = req.url().clone();
        debug!("{} querying", &url);

        let response = self.client.execute(req).await?;

        let status = response.status();
        debug!("{} done querying ({:?})", &url, status);

        if status.is_success() {
            Ok(response)
        } else {
            Err(PortalError::HttpStatus(status))
        }
    }

    async fn build_get(&self, path: &str) -> Result<Request, PortalError> {
        Ok(self
            .client
            .get(&format!("{}{}", self.base_url, path))
            .timeout(Duration::from_secs(10))
            .build()?)
    }

    async fn build_post<T>(&self, path: &str, payload: T) -> Result<Request, PortalError>
    where
        T: Serialize,
    {
        Ok(self
            .client
            .post(&format!("{}{}", self.base_url, path))
            .json(&payload)
            .timeout(Duration::from_secs(10))
            .build()?)
    }

    fn response_to_tokens(&self, response: Response) -> Result<Tokens, PortalError> {
        if let Some(auth) = response.headers().get("authorization") {
            let token = auth.to_str()?.to_owned();
            Ok(Tokens { token })
        } else {
            Err(PortalError::NoAuthorizationHeader)
        }
    }

    pub async fn login(&self, login: LoginPayload) -> Result<Tokens, PortalError> {
        let req = self.build_post("/login", login).await?;
        let response = self.execute_req(req).await?;

        self.response_to_tokens(response)
    }

    pub async fn query_status(&self) -> Result<(), PortalError> {
        let req = self.build_get("/status").await?;
        let response = self.execute_req(req).await?;
        let _bytes = response.bytes().await?;

        Ok(())
    }

    pub async fn use_refresh_token(&self, refresh_token: &str) -> Result<Tokens, PortalError> {
        let req = self
            .build_post("/refresh", json!({ "refreshToken": refresh_token }))
            .await?;
        let response = self.execute_req(req).await?;

        self.response_to_tokens(response)
    }

    pub async fn available_firmware(&self) -> Result<Vec<Firmware>> {
        let req = self.build_get("/firmware").await?;
        let response = self.execute_req(req).await?;
        let firmwares: Firmwares = response.json().await?;

        Ok(firmwares.firmwares)
    }

    pub fn to_authenticated(&self, token: Tokens) -> Result<AuthenticatedClient, PortalError> {
        AuthenticatedClient::new(&self.base_url, token)
    }
}

#[derive(Debug, Deserialize)]
struct Firmwares {
    firmwares: Vec<Firmware>,
}

#[derive(Debug, Deserialize)]
pub struct Firmware {
    pub id: i64,
    #[serde(deserialize_with = "deserialize_firmware_time")]
    pub time: DateTime<Utc>,
    pub etag: String,
    pub module: String,
    pub profile: String,
    pub version: String,
    pub url: String,
    #[serde(rename = "buildNumber")]
    pub build_number: i64,
    #[serde(rename = "buildTime", deserialize_with = "from_ts")]
    pub build_time: DateTime<Utc>,
    pub meta: HashMap<String, String>,
}

fn deserialize_firmware_time<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S.%f +0000")
        .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S.%f +0000 +0000"))
        .map_err(serde::de::Error::custom)
        .map(|v| DateTime::from_utc(v, Utc))
}

#[derive(Debug)]
pub struct BytesUploaded {
    pub bytes_uploaded: u64,
    pub total_bytes: u64,
}

impl BytesUploaded {
    pub fn completed(&self) -> bool {
        self.bytes_uploaded >= self.total_bytes
    }
}

pub trait CreatesFromBytesUploaded<M>: Send + Sync {
    fn from(&self, bytes: BytesUploaded) -> M;
}

#[derive(Debug)]
pub struct AuthenticatedClient {
    tokens: Tokens,
    plain: Client,
}

impl AuthenticatedClient {
    pub fn new(base_url: &str, tokens: Tokens) -> Result<Self, PortalError> {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", tokens.token.parse()?);

        Ok(Self {
            tokens,
            plain: Client::new_with_headers(base_url, headers)?,
        })
    }

    pub async fn query_ourselves(&self) -> Result<User, PortalError> {
        let req = self.plain.build_get("/user").await?;
        let response = self.plain.execute_req(req).await?;
        Ok(response.json().await?)
    }

    pub async fn issue_transmission_token(&self) -> Result<TransmissionToken, PortalError> {
        let req = self.plain.build_get("/user/transmission-token").await?;
        let response = self.plain.execute_req(req).await?;
        Ok(response.json().await?)
    }

    pub async fn upload_readings<M>(
        &self,
        path: &Path,
        progress: Sender<M>,
        factory: impl CreatesFromBytesUploaded<M> + 'static,
    ) -> Result<(), PortalError>
    where
        M: std::fmt::Debug + Send + Sync + 'static,
    {
        use tokio_stream::StreamExt;

        let json_path = PathBuf::from(format!("{}.json", path.display()));
        let file_meta = FileMeta::load_from_json(&json_path).await?;

        let header_map: HeaderMap = file_meta
            .headers
            .into_iter()
            .map(|(k, v)| {
                Ok((
                    HeaderName::from_lowercase(k.to_lowercase().as_bytes())?,
                    HeaderValue::from_str(&v)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .collect();

        info!("headers {:?}", &header_map);

        let file = File::open(path).await?;
        let md = file.metadata().await?;
        let total_bytes = md.len();

        let mut uploaded = 0;

        let mut reader_stream = ReaderStream::new(file);
        let async_stream = async_stream::stream! {
            while let Some(chunk) = reader_stream.next().await {
                if let Ok(chunk) = &chunk {
                    uploaded = std::cmp::min(uploaded + (chunk.len() as u64), total_bytes);

                    let bytes_uploaded = BytesUploaded { bytes_uploaded: uploaded, total_bytes };
                    if let Err(e) = progress.send(factory.from( bytes_uploaded)).await {
                        warn!("Progress receiver error: {:?}", e);
                    }
                }
                yield chunk;
            }
        };

        let url = format!("{}{}", self.plain.base_url, "/ingestion");

        info!(%url, "uploading {} bytes", total_bytes);

        let response = reqwest::Client::new()
            .post(&url)
            .headers(header_map)
            .header("content-type", "application/octet-stream")
            .header("content-length", format!("{}", total_bytes))
            .header("authorization", self.tokens.token.clone())
            .body(reqwest::Body::wrap_stream(async_stream))
            .send()
            .await?;

        let body: serde_json::Value = response.json().await?;

        info!("{:?}", body);

        Ok(())
    }
}

#[derive(Clone)]
pub struct Tokens {
    pub token: String,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Token").field(&"SECRET").finish()
    }
}

#[derive(Debug, Serialize)]
pub struct LoginPayload {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: u64,
    pub email: String,
    pub name: String,
    pub bio: String,
    pub photo: Photo,
}

#[derive(Debug, Deserialize)]
pub struct Photo {
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TransmissionToken {
    pub token: String,
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DecodedToken {
    pub email: String,
    pub exp: i64,
    pub iat: i64,
    pub refresh_token: String,
    pub scopes: Vec<String>,
    pub sub: u64,
}

impl DecodedToken {
    pub fn decode(token: &str) -> Result<DecodedToken, PortalError> {
        // Sorry, not sorry.
        token
            .split(".")
            .skip(1)
            .take(1)
            .map(|p| {
                Ok(std::str::from_utf8(&general_purpose::STANDARD_NO_PAD.decode(p)?)?.to_owned())
            })
            .collect::<Result<Vec<_>, PortalError>>()?
            .into_iter()
            .map(|p| Ok(serde_json::from_str::<DecodedToken>(p.as_str())?))
            .collect::<Result<Vec<_>, PortalError>>()?
            .into_iter()
            .next()
            .ok_or(PortalError::UnexpectedError)
    }

    pub fn issued(&self) -> DateTime<Utc> {
        Utc.timestamp_opt(self.iat, 0).unwrap()
    }

    pub fn expires(&self) -> DateTime<Utc> {
        Utc.timestamp_opt(self.exp, 0).unwrap()
    }

    pub fn remaining(&self) -> chrono::Duration {
        self.expires() - Utc::now()
    }
}

#[derive(Error, Debug)]
pub enum PortalError {
    #[error("HTTP error")]
    Request(#[from] reqwest::Error),
    #[error("IO error")]
    Io(#[from] std::io::Error),
    #[error("Invalid header value")]
    InvalidHeaderValue(#[from] InvalidHeaderValue),
    #[error("Conversion error")]
    ConversionError(#[from] ToStrError),
    #[error("HTTP status error")]
    HttpStatus(StatusCode),
    #[error("Base 64 error")]
    Base64Error(#[from] base64::DecodeError),
    #[error("UTF 8 error")]
    Utf8Error(#[from] std::str::Utf8Error),
    #[error("JSON error")]
    JsonError(#[from] serde_json::Error),
    #[error("Unexpected error")]
    UnexpectedError,
    #[error("Expected authorization header")]
    NoAuthorizationHeader,
    #[error("General error")]
    General(#[from] anyhow::Error),
}
