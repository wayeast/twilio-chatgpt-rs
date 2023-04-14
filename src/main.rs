mod texttospeech_v1_types;
mod twilio_types;

use crate::twilio_types::*;

use axum::{
    body::StreamBody,
    extract::{
        connect_info::ConnectInfo,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Host, Path, State,
    },
    http::{header, HeaderMap},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use base64::{alphabet, engine, read, write, Engine};
use futures_util::{
    sink::SinkExt,
    stream::{self, SplitSink, SplitStream, Stream, StreamExt},
};
use gcs_common::yup_oauth2;
use std::env;
use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use texttospeech_v1_types::{
    AudioConfig, AudioConfigAudioEncoding, SynthesisInput, SynthesizeSpeechRequest, TextService,
    TextSynthesizeParams, VoiceSelectionParams, VoiceSelectionParamsSsmlGender,
};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tokio_util::io::ReaderStream;

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(app_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| socket_handler(socket, app_state))
}

async fn socket_handler(mut socket: WebSocket, app_state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();
    let (sid_sink, sid_src) = oneshot::channel();

    let receive_task = tokio::spawn(hear_stuff(receiver, sid_sink));
    let send_task = tokio::spawn(say_something(sender, sid_src, app_state));
}

async fn hear_stuff(mut receiver: SplitStream<WebSocket>, sid_sink: oneshot::Sender<String>) {
    let mut stream_sid = String::new();
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(json)) => match serde_json::from_str(&json) {
                Ok(message) => match message {
                    TwilioMessage::Connected { protocol, version } => {
                        println!("Got connected message with {protocol} and {version}");
                    }
                    TwilioMessage::Start {
                        start:
                            StartMeta {
                                stream_sid: meta_sid,
                                ..
                            },
                        stream_sid: msg_sid,
                        ..
                    } => {
                        println!("Got start message with stream sid's {meta_sid} and {msg_sid}");
                        stream_sid = meta_sid;
                        break;
                    }
                    _ => {
                        println!("Hm, got media (or stop, or mark) messages before we were expecting them.");
                    }
                },
                Err(e) => {
                    println!("Error deserializing twilio text message: {e}");
                }
            },
            Ok(_) => {
                println!("Got an unsupported message type from Twilio.");
            }
            Err(e) => {
                println!("Error getting message from Twilio: {e}");
            }
        }
    }
    if !stream_sid.is_empty() {
        sid_sink.send(stream_sid).unwrap();
    }

    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(json)) => {
                match serde_json::from_str(&json) {
                    Ok(message) => match message {
                        TwilioMessage::Media {
                            sequence_number,
                            stream_sid,
                            ..
                        } => {
                            // println!("Got media message {sequence_number} for {stream_sid}");
                        }
                        TwilioMessage::Stop {
                            sequence_number, ..
                        } => {
                            println!("Got stop message {sequence_number}");
                        }
                        TwilioMessage::Mark { .. } => {
                            println!("Got mark message.");
                        }
                        _ => {
                            println!("We should not be getting Connected or Start messages now!");
                        }
                    },
                    Err(e) => println!("Failed to parse incoming text message: {e}"),
                }
            }
            Ok(_) => {
                println!("Got an unsupported message type from Twilio.");
            }
            Err(e) => {
                println!("Failed to receive message from Twilio stream: {e}");
            }
        }
    }
}

const USE_GOOGLE_TTS: bool = true;
const WRITE_SHIT_TO_FILES: bool = true;
async fn say_something(
    mut sender: SplitSink<WebSocket, Message>,
    sid_src: oneshot::Receiver<String>,
    app_state: Arc<AppState>,
) {
    let res = sid_src.await;
    if res.is_err() {
        println!("sid_sink dropped");
        return;
    }
    let stream_sid = res.unwrap();
    println!("say_something got stream sid {stream_sid}");

    sleep(Duration::from_secs(1)).await;

    let trimmed = if USE_GOOGLE_TTS {
        // See https://cloud.google.com/text-to-speech/docs/create-audio-text-command-line#synthesize_audio_from_text
        let params = TextSynthesizeParams::default();
        let audio_config = AudioConfig {
            audio_encoding: Some(AudioConfigAudioEncoding::MULAW),
            sample_rate_hertz: Some(8_000),
            ..Default::default()
        };
        let input = SynthesisInput {
            text: Some(TEST.to_string()),
            ..Default::default()
        };
        let voice = VoiceSelectionParams {
            language_code: Some("en-US".to_string()),
            name: Some("en-US-Standard-E".to_string()),
            ssml_gender: Some(VoiceSelectionParamsSsmlGender::FEMALE),
            ..Default::default()
        };
        let speech_request = SynthesizeSpeechRequest {
            audio_config: Some(audio_config),
            input: Some(input),
            voice: Some(voice),
        };
        let synthesize_response = app_state
            .gcs_client
            .synthesize(&params, &speech_request)
            .await
            .unwrap();
        let payload = synthesize_response.audio_content.unwrap();
        // `payload` is the base64-encoded mulaw/8000 bytes plus a `wav` header
        if WRITE_SHIT_TO_FILES {
            let mut file = tokio::fs::File::create("google_payload.txt").await.unwrap();
            file.write_all(payload.as_bytes()).await.unwrap();
        }
        // base64-decode `payload`
        let mut enc = Cursor::new(payload);
        let mut decoder = read::DecoderReader::new(&mut enc, &engine::general_purpose::STANDARD);
        let mut body = Vec::new();
        decoder.read_to_end(&mut body).unwrap();
        // `body` is now raw u8's; if written to a file on disk, `ffprobe` and `soxi` recognize it as
        // mulaw/8000 audio; `play` can play it.
        if WRITE_SHIT_TO_FILES {
            let mut file = tokio::fs::File::create("google_payload_decoded.wav")
                .await
                .unwrap();
            file.write_all(&body[..]).await.unwrap();
        }
        // Trim `wav` header from Google's response
        let trimmed = body[44..].to_vec();
        // `trimmed` is headerless mulaw/8000 audio; if written to a disk, it can be imported into
        // Audacity as mulaw-encoded, 8000Hz audio and played.
        if WRITE_SHIT_TO_FILES {
            let mut file = tokio::fs::File::create("google_payload-headerless.dat")
                .await
                .unwrap();
            file.write_all(&trimmed[..]).await.unwrap();
        }
        trimmed
    } else {
        // Create this file using audacity to record a mulaw/8000, single channel, wav file and
        // removing the first 44 bytes
        let mut file = tokio::fs::File::open("standard.dat").await.unwrap();
        let mut trimmed = vec![];
        file.read_to_end(&mut trimmed).await.unwrap();
        trimmed
    };

    // base64-encode the trimmed raw audio
    let re_encoded: String = engine::general_purpose::STANDARD.encode(trimmed);
    // Construct a Media message to send to Twilio.
    let outbound_media_meta = OutboundMediaMeta {
        payload: re_encoded,
    };
    let outbound_media = TwilioOutbound::Media {
        media: outbound_media_meta,
        stream_sid: stream_sid.clone(),
    };
    let json = serde_json::to_string(&outbound_media).unwrap();
    if WRITE_SHIT_TO_FILES {
        let mut file = tokio::fs::File::create("twilio_media_message.json")
            .await
            .unwrap();
        file.write_all(json.as_bytes()).await.unwrap();
    }
    // We can verify that the json content is of the right format for a Media message to be
    // consumed by Twilio.
    // println!("{json}");

    let message = Message::Text(json);
    sender.send(message).await.unwrap();
}

const TEST: &str = r#"Now is the winter of our discontent
Made glorious summer by this sun of York.
Some are born great, some achieve greatness
And some have greatness thrust upon them.
Friends, Romans, countrymen - lend me your ears!
"#;

async fn play_handler(State(app_state): State<Arc<AppState>>) -> impl IntoResponse {
    let params = TextSynthesizeParams::default();
    let audio_config = AudioConfig {
        audio_encoding: Some(AudioConfigAudioEncoding::MP3),
        ..Default::default()
    };
    let input = SynthesisInput {
        text: Some(TEST.to_string()),
        ..Default::default()
    };
    let voice = VoiceSelectionParams {
        language_code: Some("en-US".to_string()),
        name: Some("en-US-Standard-E".to_string()),
        ssml_gender: Some(VoiceSelectionParamsSsmlGender::FEMALE),
        ..Default::default()
    };
    let speech_request = SynthesizeSpeechRequest {
        audio_config: Some(audio_config),
        input: Some(input),
        voice: Some(voice),
    };
    let synthesize_response = app_state
        .gcs_client
        .synthesize(&params, &speech_request)
        .await
        .unwrap();
    let mut enc = Cursor::new(synthesize_response.audio_content.unwrap());
    let mut decoder = read::DecoderReader::new(&mut enc, &engine::general_purpose::STANDARD);
    let mut body = Vec::new();
    decoder.read_to_end(&mut body).unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "audio/mpeg".parse().unwrap());

    (headers, body)
}

async fn twiml_start_connect(Host(host): Host) -> impl IntoResponse {
    let say_action = SayAction {
        text: "Hi. I'm your Twilio host. Welcome!".to_string(),
        ..Default::default()
    };
    let url = format!("wss://{}/connect", host);
    let stream_action = StreamAction {
        url,
        track: Some(StreamTrack::Inbound),
        ..Default::default()
    };
    let connect_action = ConnectAction {
        connection: Connection::Stream(stream_action),
    };
    let response = Response {
        actions: vec![
            ResponseAction::Say(say_action),
            ResponseAction::Connect(connect_action),
        ],
    };

    let twiml = wrap_twiml(xmlserde::xml_serialize(response));
    println!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (headers, twiml)
}

async fn twiml_start_play(Host(host): Host) -> impl IntoResponse {
    let url = format!("https://{}/play", host);
    let play_action = PlayAction {
        url,
        ..Default::default()
    };
    let response = Response {
        actions: vec![ResponseAction::Play(play_action)],
    };
    let twiml = wrap_twiml(xmlserde::xml_serialize(response));
    println!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (headers, twiml)
}

struct AppState {
    base_file_dir: PathBuf,
    gcs_client: TextService,
}

async fn gcs_client() -> TextService {
    let gcs_credentials = env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .expect("No google application credentials location set.");
    let conn = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_or_http()
        .enable_http2()
        .build();
    let tls_client = hyper::Client::builder().build(conn);
    let service_account_key = yup_oauth2::read_service_account_key(&gcs_credentials)
        .await
        .expect("failed to read GCS account key");
    let gcs_authenticator = yup_oauth2::ServiceAccountAuthenticator::builder(service_account_key)
        .hyper_client(tls_client.clone())
        .persist_tokens_to_disk("tokencache.json")
        .build()
        .await
        .expect("ServiceAccount authenticator failed.");
    TextService::new(tls_client, Arc::new(gcs_authenticator))
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().unwrap();
    let base_file_dir = env::var("BASE_FILE_DIR").expect("No BASE_FILE_DIR set in env.");
    let base_file_dir = PathBuf::from(base_file_dir);

    let gcs_client = gcs_client().await;

    let app_state = Arc::new(AppState {
        base_file_dir,
        gcs_client,
    });

    let app = Router::new()
        .route("/connect", get(ws_handler))
        .route("/play", get(play_handler))
        // Choose whether to use a Play verb or Connect verb in start Twiml.
        // .route("/twilio/twiml/start", post(twiml_start_play))
        .route("/twilio/twiml/start", post(twiml_start_connect))
        .route("/", get(|| async { "Hello, World!" }))
        .with_state(app_state);

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}
