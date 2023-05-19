use anyhow::Result;
use reqwest::{header::HeaderMap, Request, Response};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use tracing::debug;

pub use reqwest::{header::InvalidHeaderValue, header::ToStrError, StatusCode};

#[derive(Debug)]
pub struct Client {
    base_url: String,
    client: reqwest::Client,
}

impl Client {
    pub fn new() -> Result<Self, PortalError> {
        Self::new_with_headers(HeaderMap::new())
    }

    pub fn new_with_headers(mut headers: HeaderMap) -> Result<Self, PortalError> {
        let sdk_version = std::env!("CARGO_PKG_VERSION");
        let user_agent = format!("rustfk ({})", sdk_version);
        headers.insert("user-agent", user_agent.parse()?);

        let client = reqwest::ClientBuilder::new()
            .user_agent("rustfk")
            .timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()?;

        let base_url = "https://api.fieldkit.org".to_owned();

        Ok(Self { base_url, client })
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

    pub async fn login(&self, login: LoginPayload) -> Result<Option<Tokens>, PortalError> {
        let req = self.build_post("/login", login).await?;
        let response = self.execute_req(req).await?;

        if let Some(auth) = response.headers().get("authorization") {
            let token = auth.to_str()?.to_owned();
            Ok(Some(Tokens {
                token,
                refresh: None,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn query_status(&self) -> Result<(), PortalError> {
        let req = self.build_get("/status").await?;
        let response = self.execute_req(req).await?;
        let _bytes = response.bytes().await?;

        Ok(())
    }

    pub fn to_authenticated(&self, token: Tokens) -> Result<AuthenticatedClient, PortalError> {
        AuthenticatedClient::new(token)
    }
}

#[derive(Debug)]
pub struct AuthenticatedClient {
    plain: Client,
}

impl AuthenticatedClient {
    pub fn new(token: Tokens) -> Result<Self, PortalError> {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", token.token.parse()?);

        Ok(Self {
            plain: Client::new_with_headers(headers)?,
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
}

pub struct Tokens {
    pub token: String,
    pub refresh: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct TransmissionToken {
    pub token: String,
    pub url: String,
}

#[derive(Error, Debug)]
pub enum PortalError {
    #[error("HTTP error")]
    Request(#[from] reqwest::Error),
    #[error("Invalid header value")]
    InvalidHeaderValue(#[from] InvalidHeaderValue),
    #[error("Conversion error")]
    ConversionError(#[from] ToStrError),
    #[error("HTTP status error")]
    HttpStatus(StatusCode),
}
