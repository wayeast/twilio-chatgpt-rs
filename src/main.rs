use axum::{
    body::StreamBody,
    extract::{Host, Path, State},
    http::{header, HeaderMap},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde::Serialize;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio_util::io::ReaderStream;

#[derive(Serialize)]
struct Response {
    #[serde(rename = "$value")]
    actions: Vec<ResponseAction>,
}

#[derive(Serialize)]
enum ResponseAction {
    Play(PlayAction),
}

#[derive(Serialize)]
struct PlayAction {
    #[serde(rename = "$value")]
    url: String,
}

async fn play_handler(
    Path(id): Path<String>,
    State(app_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let actual = app_state.base_file_dir.join(id);
    let f = File::open(actual).await.unwrap();
    let stream = ReaderStream::new(f);
    let stream_body = StreamBody::new(stream);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "audio/mpeg".parse().unwrap());

    (headers, stream_body)
}

async fn twiml_start(Host(host): Host) -> impl IntoResponse {
    let url = format!("https://{}/play/ungarble_test_chunk_000000002.mp3", host);
    let play_action = PlayAction { url };
    let response_action = ResponseAction::Play(play_action);
    let actions = vec![response_action];
    let response = Response { actions };
    let twiml = serde_xml_rs::to_string(&response).unwrap();
    println!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (headers, twiml)
}

struct AppState {
    base_file_dir: PathBuf,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().unwrap();
    let base_file_dir = env::var("BASE_FILE_DIR").expect("No BASE_FILE_DIR set in env.");
    let base_file_dir = PathBuf::from(base_file_dir);

    let app_state = Arc::new(AppState { base_file_dir });

    let app = Router::new()
        .route("/play/:id", get(play_handler))
        .route("/twilio/twiml/start", post(twiml_start))
        .route("/", get(|| async { "Hello, World!" }))
        .with_state(app_state);

    axum::Server::bind(&"0.0.0.0:8080".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}
