use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use nonprofit_nightly_index::{InfraiClient, RecordKind, ServiceError, Source};
use serde_json::{json, Value};
use std::{env, sync::Arc};

#[derive(Clone)]
struct AppState {
    client: InfraiClient,
    sources: Arc<Vec<Source>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = env::args().nth(1).unwrap_or_else(|| "serve".to_owned());
    let client = InfraiClient::from_env()?;
    let sources = sources_from_env()?;

    match command.as_str() {
        "bootstrap" => {
            client.create_collection().await?;
            let public_base_url = required("PUBLIC_BASE_URL")?;
            let job_id = client.install_nightly_schedule(&public_base_url).await?;
            println!("collection ready; nightly job {job_id} installed");
        }
        "preview" => {
            let records = client.preview(&sources).await?;
            println!(
                "prepared {} nonprofit records; no persistent resources created",
                records.len()
            );
        }
        "reindex" => {
            let indexed = client.reindex(&sources).await?;
            println!("indexed {indexed} nonprofit records");
        }
        "serve" => serve(client, sources).await?,
        _ => return Err("command must be bootstrap, preview, reindex, or serve".into()),
    }
    Ok(())
}

async fn serve(
    client: InfraiClient,
    sources: Vec<Source>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState {
        client,
        sources: Arc::new(sources),
    };
    let app = Router::new()
        .route("/reindex", post(reindex))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    println!("listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn reindex(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    state
        .client
        .reindex(&state.sources)
        .await
        .map(|indexed| Json(json!({"indexed": indexed, "collection": "nonprofit-operations"})))
        .map_err(render_error)
}

fn render_error(error: ServiceError) -> (StatusCode, Json<Value>) {
    let status = error.client_status();
    (status, Json(json!({"error": error.to_string()})))
}

fn sources_from_env() -> Result<Vec<Source>, ServiceError> {
    Ok(vec![
        Source {
            kind: RecordKind::DonorReceipt,
            url: required("DONOR_RECEIPTS_URL")?,
        },
        Source {
            kind: RecordKind::VolunteerReminder,
            url: required("VOLUNTEER_REMINDERS_URL")?,
        },
        Source {
            kind: RecordKind::CampaignReport,
            url: required("CAMPAIGN_REPORTS_URL")?,
        },
    ])
}

fn required(name: &'static str) -> Result<String, ServiceError> {
    env::var(name).map_err(|_| ServiceError::Config(name))
}
