use std::{
    error::Error as StdError,
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use air_transcode::{
    AppState, Config, app,
    mp4::{decode_time, media_data, validate_init_segment, validate_media_segment},
};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
    routing::get,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;

#[derive(Clone)]
struct OriginState {
    bytes: Arc<Vec<u8>>,
    range_requests: Arc<AtomicUsize>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_http_transmux_is_seekable_deduplicated_and_playable() -> TestResult {
    air_transcode::initialize()?;
    let fixtures = tempfile::tempdir()?;
    let fixture = fixtures.path().join("web-compatible.mp4");
    generate_fixture(&fixture, FixtureKind::H264Aac)?;

    let origin_state = OriginState {
        bytes: Arc::new(std::fs::read(&fixture)?),
        range_requests: Arc::new(AtomicUsize::new(0)),
    };
    let origin = Router::new()
        .route("/media", get(origin_media))
        .with_state(origin_state.clone());
    let (origin_url, origin_task) = spawn(origin).await?;
    let (server_url, server_task, cache) = spawn_transcoder().await?;
    let client = reqwest::Client::new();

    let session = create_session(&client, &server_url, &origin_url).await?;
    assert_eq!(session["seekable"], true);
    assert_eq!(session["tracks"][0]["web_compatible"], true);
    assert_eq!(session["tracks"][1]["web_compatible"], true);
    assert_eq!(session["tracks"][0]["rfc6381_codec"], "avc1.42C015");
    let id = json_string(&session, "id")?;

    let manifest = client
        .get(format!("{server_url}/v1/sessions/{id}/video.m3u8"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(manifest.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    assert!(manifest.contains("#EXT-X-ENDLIST"));
    assert!(manifest.contains("video/segments/2"));

    let segment_two_url = format!("{server_url}/v1/sessions/{id}/video/segments/2");
    let responses = futures_util::future::try_join_all(
        (0..8).map(|_| fetch_bytes(&client, segment_two_url.clone())),
    )
    .await?;
    assert!(responses.windows(2).all(|pair| pair[0] == pair[1]));
    validate_media_segment(&responses[0])?;
    assert!(decode_time(&responses[0]).is_some_and(|value| value > 0));
    let metrics: Value = client
        .get(format!("{server_url}/v1/metrics"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(metrics["generated_segments"], 1);
    assert_eq!(metrics["cache_hits"], 7);

    let init = fetch_bytes(
        &client,
        format!("{server_url}/v1/sessions/{id}/video/init.mp4"),
    )
    .await?;
    let segment_one = fetch_bytes(
        &client,
        format!("{server_url}/v1/sessions/{id}/video/segments/1"),
    )
    .await?;
    validate_init_segment(&init)?;
    validate_media_segment(&segment_one)?;
    assert_eq!(decode_time(&segment_one), Some(0));
    assert!(media_data(&segment_one) != media_data(&responses[0]));
    assert!(origin_state.range_requests.load(Ordering::Relaxed) > 0);

    let cached_segment = cache
        .path()
        .join(id)
        .join("video")
        .join("1")
        .join("segment.m4s");
    std::fs::write(cached_segment, b"corrupt")?;
    let repaired = fetch_bytes(
        &client,
        format!("{server_url}/v1/sessions/{id}/video/segments/1"),
    )
    .await?;
    validate_media_segment(&repaired)?;

    play_hls(format!("{server_url}/v1/sessions/{id}/master.m3u8")).await?;
    origin_task.abort();
    server_task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_invalid_sources_media_and_ranges() -> TestResult {
    air_transcode::initialize()?;
    let origin_state = OriginState {
        bytes: Arc::new(vec![0xAA; 4096]),
        range_requests: Arc::new(AtomicUsize::new(0)),
    };
    let origin = Router::new()
        .route("/media", get(origin_media))
        .with_state(origin_state);
    let (origin_url, origin_task) = spawn(origin).await?;
    let (server_url, server_task, _cache) = spawn_transcoder().await?;
    let client = reqwest::Client::new();

    let invalid_scheme = client
        .post(format!("{server_url}/v1/sessions"))
        .json(&json!({"source": {"url": "ftp://example.invalid/video.mkv"}}))
        .send()
        .await?;
    assert_eq!(invalid_scheme.status(), StatusCode::BAD_REQUEST);

    let invalid_header = client
        .post(format!("{server_url}/v1/sessions"))
        .json(&json!({
            "source": {
                "url": format!("{origin_url}/media"),
                "headers": {"Bad Header": "value"}
            }
        }))
        .send()
        .await?;
    assert_eq!(invalid_header.status(), StatusCode::BAD_REQUEST);

    let malformed_media = client
        .post(format!("{server_url}/v1/sessions"))
        .json(&json!({
            "source": {
                "url": format!("{origin_url}/media"),
                "headers": {"Authorization": "Bearer fixture"}
            }
        }))
        .send()
        .await?;
    assert_eq!(malformed_media.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let unknown = client
        .get(format!(
            "{server_url}/v1/sessions/00000000-0000-0000-0000-000000000000"
        ))
        .send()
        .await?;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    origin_task.abort();
    server_task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_vp9_opus_is_transcoded_to_web_cmaf() -> TestResult {
    air_transcode::initialize()?;
    let fixtures = tempfile::tempdir()?;
    let fixture = fixtures.path().join("incompatible.mkv");
    generate_fixture(&fixture, FixtureKind::Vp9Opus)?;

    let origin_state = OriginState {
        bytes: Arc::new(std::fs::read(&fixture)?),
        range_requests: Arc::new(AtomicUsize::new(0)),
    };
    let origin = Router::new()
        .route("/media", get(origin_media))
        .with_state(origin_state);
    let (origin_url, origin_task) = spawn(origin).await?;
    let (server_url, server_task, _cache) = spawn_transcoder().await?;
    let client = reqwest::Client::new();

    let session = create_session(&client, &server_url, &origin_url).await?;
    assert_eq!(session["tracks"][0]["web_compatible"], false);
    assert_eq!(session["tracks"][1]["web_compatible"], false);
    let id = json_string(&session, "id")?;
    let master = client
        .get(format!("{server_url}/v1/sessions/{id}/master.m3u8"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(!master.contains("CODECS="));

    for track in ["video", "audio"] {
        let init = fetch_bytes(
            &client,
            format!("{server_url}/v1/sessions/{id}/{track}/init.mp4"),
        )
        .await?;
        let first = fetch_bytes(
            &client,
            format!("{server_url}/v1/sessions/{id}/{track}/segments/1"),
        )
        .await?;
        let second = fetch_bytes(
            &client,
            format!("{server_url}/v1/sessions/{id}/{track}/segments/2"),
        )
        .await?;
        validate_init_segment(&init)?;
        validate_media_segment(&first)?;
        validate_media_segment(&second)?;
        assert_eq!(decode_time(&first), Some(0));
        assert!(decode_time(&second).is_some_and(|value| value > 0));
    }

    play_hls(format!("{server_url}/v1/sessions/{id}/master.m3u8")).await?;
    origin_task.abort();
    server_task.abort();
    Ok(())
}

async fn create_session(
    client: &reqwest::Client,
    server_url: &str,
    origin_url: &str,
) -> TestResult<Value> {
    Ok(client
        .post(format!("{server_url}/v1/sessions"))
        .json(&json!({
            "source": {
                "url": format!("{origin_url}/media"),
                "headers": { "Authorization": "Bearer fixture" }
            },
            "output": { "max_width": 1920 }
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn fetch_bytes(client: &reqwest::Client, url: String) -> TestResult<Vec<u8>> {
    let response = client.get(url).send().await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        return Err(io::Error::other(format!(
            "segment request failed with {status}: {}",
            String::from_utf8_lossy(&body)
        ))
        .into());
    }
    Ok(body.to_vec())
}

fn json_string<'a>(value: &'a Value, key: &str) -> TestResult<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("missing {key}")))
        .map_err(Into::into)
}

async fn spawn(router: Router) -> TestResult<(String, JoinHandle<io::Result<()>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok((format!("http://{address}"), task))
}

async fn spawn_transcoder() -> TestResult<(String, JoinHandle<io::Result<()>>, TempDir)> {
    let cache = tempfile::tempdir()?;
    let config = Config {
        bind: "127.0.0.1:0".parse()?,
        cache_dir: cache.path().to_owned(),
        segment_seconds: 2,
        max_sessions: 4,
        max_pipelines: 2,
        max_cached_segments: 8,
        session_ttl_seconds: 30,
        probe_timeout_seconds: 10,
    };
    let state = AppState::new(config)?;
    let (url, task) = spawn(app(state)).await?;
    Ok((url, task, cache))
}

async fn origin_media(
    State(state): State<OriginState>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    if headers.get(header::AUTHORIZATION) != Some(&HeaderValue::from_static("Bearer fixture")) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let total = state.bytes.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'));
    let (status, start, end) = if let Some((start, end)) = range {
        state.range_requests.fetch_add(1, Ordering::Relaxed);
        let start = start
            .parse::<usize>()
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        let end = if end.is_empty() {
            total.saturating_sub(1)
        } else {
            end.parse::<usize>().map_err(|_| StatusCode::BAD_REQUEST)?
        }
        .min(total.saturating_sub(1));
        if start > end || start >= total {
            return Err(StatusCode::RANGE_NOT_SATISFIABLE);
        }
        (StatusCode::PARTIAL_CONTENT, start, end)
    } else {
        (StatusCode::OK, 0, total.saturating_sub(1))
    };
    let body = state
        .bytes
        .get(start..=end)
        .ok_or(StatusCode::RANGE_NOT_SATISFIABLE)?
        .to_vec();
    let mut builder = Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, body.len());
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }
    builder
        .body(Body::from(body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Clone, Copy)]
enum FixtureKind {
    H264Aac,
    Vp9Opus,
}

fn generate_fixture(path: &Path, kind: FixtureKind) -> TestResult {
    let mux_and_video = match kind {
        FixtureKind::H264Aac => format!(
            "mp4mux name=mux ! filesink location={} videotestsrc num-buffers=180 pattern=smpte horizontal-speed=3 ! video/x-raw,format=I420,width=320,height=180,framerate=30/1 ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=30 bframes=0 byte-stream=false ! h264parse ! queue ! mux.",
            path.display()
        ),
        FixtureKind::Vp9Opus => format!(
            "matroskamux name=mux ! filesink location={} videotestsrc num-buffers=180 pattern=ball animation-mode=frames ! video/x-raw,width=320,height=180,framerate=30/1 ! vp9enc deadline=1 keyframe-max-dist=30 ! queue ! mux.",
            path.display()
        ),
    };
    let audio = match kind {
        FixtureKind::H264Aac => {
            "audiotestsrc num-buffers=282 wave=white-noise ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! queue ! mux."
        }
        FixtureKind::Vp9Opus => {
            "audiotestsrc num-buffers=282 wave=ticks ! audio/x-raw,rate=48000,channels=2 ! opusenc ! queue ! mux."
        }
    };
    run_pipeline(&format!("{mux_and_video} {audio}"), Duration::from_secs(20))
}

async fn play_hls(uri: String) -> TestResult {
    tokio::task::spawn_blocking(move || -> TestResult {
        let playbin = gst::ElementFactory::make("playbin")
            .property("uri", uri)
            .build()?;
        let video_sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()?;
        let audio_sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()?;
        playbin.set_property("video-sink", video_sink);
        playbin.set_property("audio-sink", audio_sink);
        run_element(&playbin, Duration::from_secs(30))
    })
    .await??;
    Ok(())
}

fn run_pipeline(description: &str, timeout: Duration) -> TestResult {
    let element = gst::parse::launch(description)?;
    run_element(&element, timeout)
}

fn run_element(element: &gst::Element, timeout: Duration) -> TestResult {
    element.set_state(gst::State::Playing)?;
    let bus = element
        .bus()
        .ok_or_else(|| io::Error::other("GStreamer element has no bus"))?;
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_nseconds(u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX)),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "GStreamer pipeline timed out"))?;
    let result = match message.view() {
        gst::MessageView::Eos(_) => Ok(()),
        gst::MessageView::Error(error) => Err(io::Error::other(format!(
            "GStreamer pipeline failed: {} ({:?})",
            error.error(),
            error.debug()
        ))),
        _ => Err(io::Error::other("unexpected GStreamer message")),
    };
    let _ = element.set_state(gst::State::Null);
    result.map_err(Into::into)
}
