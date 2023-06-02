use anyhow::Result;
use base64::{engine::general_purpose, Engine};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::{header::HeaderMap, Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
}
