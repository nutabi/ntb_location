use std::str::FromStr;
use std::fmt::Display;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};
use axum::routing::{get, post};
use chrono::NaiveDateTime;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::SqlitePool;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{prelude::*, fmt, EnvFilter};

/// Regex to sanitise strings.
/// 
/// It will be called multiple times so we can make it a static variable.
static SANITISATION_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-zA-Z0-9_ ]+$").unwrap()
});

/// Custom deserializer for empty strings
/// 
/// This is to ensure proper parsing and writing SQL queries, sicne empty strings imply
/// the users does not want to filter by that field, not actually filtering by an empty string.
fn empty_string_as_none<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    let opt = Option::<String>::deserialize(de)?;
    match opt.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => T::from_str(s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Helper function to sanitise strings.
fn sanitise_string(s: &str) -> bool {
    SANITISATION_REGEX.is_match(s)
}

/// Data structure for location data returned from/inserted into the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DbLocData {
    id: i64,
    source: String,
    latitude: f64,
    longitude: f64,
    created_at: NaiveDateTime,
}

/// Data structure for location data sent by the client to `POST /` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PostLocData {
    source: String,
    latitude: f64,
    longitude: f64,
}

/// Data structure for query parameters sent by the client to `GET /` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GetLocQuery {
    #[serde(default, deserialize_with = "empty_string_as_none")]
    source: Option<String>,

    #[serde(default, deserialize_with = "empty_string_as_none")]
    from: Option<NaiveDateTime>,

    #[serde(default, deserialize_with = "empty_string_as_none")]
    to: Option<NaiveDateTime>,
}

/// Application state.
/// 
/// Contains the database pool. It may also contains more items in the future.
#[derive(Debug, Clone)]
struct App {
    database_pool: SqlitePool,
}

/// Handler for `POST /` endpoint.
/// 
/// It will insert a new location record into the database as requested by the client.
async fn post_location(State(app): State<App>, Json(data): Json<PostLocData>) -> impl IntoResponse {
    tracing::info!("POST / <- {:?}", data);

    if !sanitise_string(&data.source) {
        return (StatusCode::BAD_REQUEST, "Invalid source").into_response();
    }

    let result = sqlx::query_as!(
        DbLocData,
        "INSERT INTO locations (source, latitude, longitude) VALUES (?, ?, ?) RETURNING *",
        data.source,
        data.latitude,
        data.longitude,
    ).fetch_one(&app.database_pool).await;

    if let Ok(data) = result {
        tracing::info!("Record added: {:?}", data);
        return (StatusCode::OK, "Record added").into_response();
    } else {
        tracing::error!("Cannot add record: {:?}", result);
        return (StatusCode::INTERNAL_SERVER_ERROR, "No record added").into_response();
    }
}

/// Handler for `GET /` endpoint.
/// 
/// It will fetch all location records from the database as requested by the client. Optional parameters
/// are used to filter the records.
async fn get_all_locations(State(app): State<App>, Query(query): Query<GetLocQuery>) -> impl IntoResponse {
    tracing::info!("GET / <- {:?}", query);

    if let Some(ref s) = query.source {
        if !sanitise_string(s) {
            return (StatusCode::BAD_REQUEST, "Invalid source").into_response();
        }
    }

    let result = sqlx::query_as!(
        DbLocData,
        r#"
        SELECT * FROM locations
        WHERE 
            (?1 IS NULL OR source = ?1)
            AND (?2 IS NULL OR created_at >= ?2)
            AND (?3 IS NULL OR created_at <= ?3)
        "#,
        query.source,
        query.from,
        query.to,
    )
    .fetch_all(&app.database_pool)
    .await;
    
    if let Ok(data) = result {
        tracing::info!("{} records fetched: {:?}", data.len(), query);
        return Json(data).into_response();
    } else {
        tracing::error!("Cannot fetch records: {:?}", result);
        return (StatusCode::INTERNAL_SERVER_ERROR, "No records fetched").into_response();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables
    #[cfg(debug_assertions)]
    dotenvy::dotenv().context("Cannot load .env file")?;

    // Set up tracing
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    // Open database connection
    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL is not set")?;
    let database_pool = SqlitePool::connect(&database_url)
        .await
        .context("Cannot connect to database")?;

    // Initialise application
    let state = App { database_pool };
    let app = Router::new()
        .route("/", get(get_all_locations))
        .route("/", post(post_location))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let port = std::env::var("PORT")
        .context("PORT is not set")?;

    // Serve application
    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Starting server on {}", addr);

    let listener = TcpListener::bind(&addr)
        .await
        .context("Cannot bind to port")?;
    axum::serve(listener, app)
        .await
        .context("Cannot start server")?;

    Ok(())
}
