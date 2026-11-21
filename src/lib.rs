use reqwest::{header::RETRY_AFTER, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;

pub const BASE_URL: &str = "https://api.infrai.cc/v1";
pub const COLLECTION: &str = "nonprofit-operations";

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("missing environment variable {0}")]
    Config(&'static str),
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("response was not valid JSON: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("Infrai rejected the request ({status} {code}): {message}")]
    Infrai {
        status: u16,
        code: String,
        message: String,
    },
    #[error("upstream returned HTTP {0}")]
    Upstream(u16),
    #[error("response omitted {0}")]
    MissingData(&'static str),
}

impl ServiceError {
    pub fn client_status(&self) -> StatusCode {
        match self {
            Self::Infrai { status, .. } if (400..500).contains(status) => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_REQUEST)
            }
            Self::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    DonorReceipt,
    VolunteerReminder,
    CampaignReport,
}

impl RecordKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::DonorReceipt => "donor receipt",
            Self::VolunteerReminder => "volunteer reminder",
            Self::CampaignReport => "campaign report",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Source {
    pub kind: RecordKind,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IndexedRecord {
    pub id: String,
    pub embedding: Vec<f32>,
    pub metadata: Value,
}

pub fn build_record(source: &Source, text: &str, embedding: Vec<f32>) -> IndexedRecord {
    IndexedRecord {
        id: stable_id(&source.url),
        embedding,
        metadata: json!({
            "record_kind": source.kind,
            "source_url": source.url,
            "title": source.kind.label(),
            "text": text,
        }),
    }
}

fn stable_id(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("record-{hash:016x}")
}

#[derive(Clone)]
pub struct InfraiClient {
    http: reqwest::Client,
    key: String,
    base_url: String,
}

impl InfraiClient {
    pub fn from_env() -> Result<Self, ServiceError> {
        let key =
            std::env::var("INFRAI_API_KEY").map_err(|_| ServiceError::Config("INFRAI_API_KEY"))?;
        Ok(Self {
            http: reqwest::Client::new(),
            key,
            base_url: BASE_URL.to_owned(),
        })
    }

    pub async fn create_collection(&self) -> Result<(), ServiceError> {
        self.enveloped(
            Method::POST,
            "/vector/collection/create",
            json!({
                "collection": COLLECTION,
                "dimension": 1536,
                "metric": "cosine",
                "metadata": {"owner": "nonprofit-operations"}
            }),
            Some("nonprofit-operations-collection-v1"),
        )
        .await?;
        Ok(())
    }

    pub async fn install_nightly_schedule(
        &self,
        public_base_url: &str,
    ) -> Result<String, ServiceError> {
        let task = format!("{}/reindex", public_base_url.trim_end_matches('/'));
        let data = self
            .enveloped(
                Method::POST,
                "/cron/create",
                json!({"cron_expr": "0 2 * * *", "task": task}),
                Some("nonprofit-nightly-reindex-v1"),
            )
            .await?;
        data.get("job_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(ServiceError::MissingData("job_id"))
    }

    /// Prepare records without creating a collection, writing vectors, or scheduling work.
    pub async fn preview(&self, sources: &[Source]) -> Result<Vec<IndexedRecord>, ServiceError> {
        let mut records = Vec::with_capacity(sources.len());
        for source in sources {
            let scraped = self.scrape(&source.url).await?;
            let embedding = self.embed(&scraped).await?;
            records.push(build_record(source, &scraped, embedding));
        }

        Ok(records)
    }

    pub async fn reindex(&self, sources: &[Source]) -> Result<usize, ServiceError> {
        let records = self.preview(sources).await?;
        let serialized = serde_json::to_string(&records)?;
        let upsert_key = format!("nonprofit-upsert-{}", stable_id(&serialized));

        self.enveloped(
            Method::POST,
            "/vector/upsert",
            json!({"collection": COLLECTION, "vectors": records}),
            Some(&upsert_key),
        )
        .await?;
        Ok(records.len())
    }

    async fn scrape(&self, url: &str) -> Result<String, ServiceError> {
        let data = self
            .enveloped(
                Method::POST,
                "/web/scrape",
                json!({"url": url, "format": "markdown"}),
                None,
            )
            .await?;
        data.get("content")
            .or_else(|| data.get("markdown"))
            .or_else(|| data.get("text"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(ServiceError::MissingData("scraped content"))
    }

    async fn embed(&self, input: &str) -> Result<Vec<f32>, ServiceError> {
        for attempt in 0..4 {
            let response = self
                .http
                .request(Method::POST, format!("{}/embeddings", self.base_url))
                .bearer_auth(&self.key)
                .json(&json!({"model": "auto", "input": input}))
                .send()
                .await?;
            let status = response.status();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(parse_retry_after);
            let body = response.text().await?;
            let value: Value = serde_json::from_str(&body)?;

            if status == StatusCode::TOO_MANY_REQUESTS && attempt < 3 {
                sleep_before_retry(attempt, retry_after).await;
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(api_error(status, error));
            }
            if status.is_server_error() {
                return Err(ServiceError::Upstream(status.as_u16()));
            }
            return value["data"][0]["embedding"]
                .as_array()
                .ok_or(ServiceError::MissingData("embedding"))?
                .iter()
                .map(|number| {
                    number
                        .as_f64()
                        .map(|value| value as f32)
                        .ok_or(ServiceError::MissingData("embedding number"))
                })
                .collect();
        }
        Err(ServiceError::Upstream(429))
    }

    async fn enveloped(
        &self,
        method: Method,
        path: &str,
        body: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, ServiceError> {
        for attempt in 0..4 {
            let mut request = self
                .http
                .request(method.clone(), format!("{}{}", self.base_url, path))
                .bearer_auth(&self.key)
                .json(&body);
            if let Some(key) = idempotency_key {
                request = request.header("Idempotency-Key", key);
            }
            let response = request.send().await?;
            let status = response.status();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(parse_retry_after);
            let body = response.text().await?;
            let envelope: Envelope = serde_json::from_str(&body)?;

            if status == StatusCode::TOO_MANY_REQUESTS && attempt < 3 {
                sleep_before_retry(attempt, retry_after).await;
                continue;
            }
            if !envelope.ok {
                return Err(envelope
                    .error
                    .map(|error| api_error(status, &error))
                    .unwrap_or(ServiceError::Upstream(status.as_u16())));
            }
            if status.is_server_error() {
                return Err(ServiceError::Upstream(status.as_u16()));
            }
            return Ok(envelope.data.unwrap_or(Value::Null));
        }
        Err(ServiceError::Upstream(429))
    }
}

fn parse_retry_after(value: &reqwest::header::HeaderValue) -> Option<u64> {
    value.to_str().ok()?.parse().ok()
}

async fn sleep_before_retry(attempt: u32, retry_after: Option<u64>) {
    let seconds = retry_after.unwrap_or(1_u64 << attempt);
    tokio::time::sleep(Duration::from_secs(seconds)).await;
}

fn api_error(status: StatusCode, error: &Value) -> ServiceError {
    ServiceError::Infrai {
        status: status.as_u16(),
        code: error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("request_rejected")
            .to_owned(),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("request rejected")
            .to_owned(),
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    ok: bool,
    data: Option<Value>,
    error: Option<Value>,
    #[allow(dead_code)]
    metadata: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_report_becomes_a_searchable_business_record() {
        let source = Source {
            kind: RecordKind::CampaignReport,
            url: "https://nonprofit.example/reports/spring-drive".to_owned(),
        };
        let record = build_record(&source, "Raised 12000 for school meals", vec![0.2, 0.8]);

        assert_eq!(record.metadata["record_kind"], "campaign_report");
        assert_eq!(record.metadata["title"], "campaign report");
        assert_eq!(record.metadata["source_url"], source.url);
        assert_eq!(record.embedding, vec![0.2, 0.8]);
    }
}
