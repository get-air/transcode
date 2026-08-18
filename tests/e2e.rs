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
    spawn_server, spawn_tauri_host,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_server_binds_ephemeral_loopback_and_shuts_down() -> TestResult {
    let cache = tempfile::tempdir()?;
    let mut config = Config::loopback(cache.path());
    config.max_pipelines = 1;
    let server = spawn_server(config).await?;
    assert!(server.address().ip().is_loopback());
    assert_ne!(server.address().port(), 0);

    let response = reqwest::get(format!("{}/health", server.origin())).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await?["engine"], "gstreamer");
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tauri_host_keeps_admin_local_and_cast_surface_media_only() -> TestResult {
    let cache = tempfile::tempdir()?;
    let host = spawn_tauri_host(Config::loopback(cache.path()), "127.0.0.1:0".parse()?).await?;
    let client = reqwest::Client::new();
    let admin_health = format!("{}/health", host.admin_origin());
    assert_eq!(
        client.get(&admin_health).send().await?.status(),
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        client
            .get(&admin_health)
            .bearer_auth(host.admin_token())
            .send()
            .await?
            .status(),
        StatusCode::OK,
    );
    let health = host
        .cast_url("127.0.0.1".parse()?, "/health")
        .ok_or_else(|| io::Error::other("cast URL is unavailable"))?;
    assert_eq!(client.get(&health).send().await?.status(), StatusCode::OK);
    assert_eq!(
        client
            .post(health.replace("/health", "/v1/sessions"))
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND,
    );
    let wrong_token = health.replacen("/cast/", "/cast/wrong-", 1);
    assert_eq!(
        client.get(wrong_token).send().await?.status(),
        StatusCode::NOT_FOUND
    );
    assert!(host.admin_origin().starts_with("http://127.0.0.1:"));
    assert!(!health.contains(host.admin_token()));
    host.shutdown().await?;
    Ok(())
}

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
        .join("0")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h264_long_gop_falls_back_to_exact_keyframe_transcode() -> TestResult {
    air_transcode::initialize()?;
    let fixtures = tempfile::tempdir()?;
    let fixture = fixtures.path().join("long-gop.mp4");
    generate_fixture(&fixture, FixtureKind::H264LongGop)?;

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
    assert_eq!(session["renditions"][0]["mode"], "transmux");
    let id = json_string(&session, "id")?;

    let segment = fetch_bytes(
        &client,
        format!("{server_url}/v1/sessions/{id}/video/segments/2"),
    )
    .await?;
    validate_media_segment(&segment)?;
    assert!(decode_time(&segment).is_some_and(|value| value > 0));

    let mut metrics = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        metrics = client
            .get(format!("{server_url}/v1/metrics"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if metrics["generated_segments"]
            .as_u64()
            .is_some_and(|n| n >= 2)
            && metrics["active_pipelines"] == 0
        {
            break;
        }
    }
    assert!(
        metrics["transcode_segments"]
            .as_u64()
            .is_some_and(|n| n >= 1)
    );
    assert_eq!(metrics["active_pipelines"], 0);
    assert_eq!(metrics["failed_pipelines"], 0);

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
    assert!(master.contains("CODECS=\"avc1.640028,mp4a.40.2\""));

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selects_an_exact_audio_track_by_discovered_index() -> TestResult {
    air_transcode::initialize()?;
    let fixtures = tempfile::tempdir()?;
    let fixture = fixtures.path().join("multi-audio.mp4");
    generate_fixture(&fixture, FixtureKind::MultiAac)?;
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

    let default_session = create_session(&client, &server_url, &origin_url).await?;
    let audio_indices = default_session["tracks"]
        .as_array()
        .ok_or_else(|| io::Error::other("session tracks are not an array"))?
        .iter()
        .filter(|track| track["kind"] == "audio")
        .filter_map(|track| track["index"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(audio_indices.len(), 2);
    let default_id = json_string(&default_session, "id")?;
    let default_audio = fetch_bytes(
        &client,
        format!("{server_url}/v1/sessions/{default_id}/audio/segments/1"),
    )
    .await?;
    let mut generated = 0_u64;
    for _ in 0..50 {
        let metrics: Value = client
            .get(format!("{server_url}/v1/metrics"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        generated = metrics["generated_segments"].as_u64().unwrap_or(0);
        if generated >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(generated >= 2, "adjacent segment was not prefetched");
    let before: Value = client
        .get(format!("{server_url}/v1/metrics"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let _prefetched = fetch_bytes(
        &client,
        format!("{server_url}/v1/sessions/{default_id}/audio/segments/2"),
    )
    .await?;
    let after: Value = client
        .get(format!("{server_url}/v1/metrics"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        after["cache_hits"].as_u64(),
        before["cache_hits"].as_u64().map(|value| value + 1)
    );

    let selected_session = create_session_with_output(
        &client,
        &server_url,
        &origin_url,
        json!({ "audio_track_index": audio_indices[1] }),
    )
    .await?;
    let selected_id = json_string(&selected_session, "id")?;
    let selected_audio = fetch_bytes(
        &client,
        format!("{server_url}/v1/sessions/{selected_id}/audio/segments/1"),
    )
    .await?;
    validate_media_segment(&default_audio)?;
    validate_media_segment(&selected_audio)?;
    assert!(media_data(&default_audio) != media_data(&selected_audio));

    origin_task.abort();
    server_task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_modern_video_codecs_are_transmuxed_without_reencoding() -> TestResult {
    air_transcode::initialize()?;
    for (kind, codec) in [(FixtureKind::H265Aac, "h265"), (FixtureKind::Av1Aac, "av1")] {
        let fixtures = tempfile::tempdir()?;
        let fixture = fixtures.path().join(format!("{codec}.mkv"));
        generate_fixture(&fixture, kind)?;
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
        let session = create_session_with_output(
            &client,
            &server_url,
            &origin_url,
            json!({
                "max_width": 1920,
                "max_height": 1080,
                "video_codecs": [codec]
            }),
        )
        .await?;
        assert_eq!(session["tracks"][0]["video_codec"], codec);
        assert_eq!(session["renditions"][0]["mode"], "transmux");
        assert_eq!(
            session["renditions"][0]["output_codec"],
            if codec == "h265" { "hvc1" } else { "av01" }
        );
        let id = json_string(&session, "id")?;
        let init = fetch_bytes(
            &client,
            format!("{server_url}/v1/sessions/{id}/video/init.mp4"),
        )
        .await
        .map_err(|error| io::Error::other(format!("{codec} init failed: {error}")))?;
        let media = fetch_bytes(
            &client,
            format!("{server_url}/v1/sessions/{id}/video/segments/1"),
        )
        .await
        .map_err(|error| io::Error::other(format!("{codec} media failed: {error}")))?;
        validate_init_segment(&init)?;
        validate_media_segment(&media)?;
        let metrics: Value = client
            .get(format!("{server_url}/v1/metrics"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(metrics["transmux_segments"], 1);
        assert_eq!(metrics["transcode_segments"], 0);
        origin_task.abort();
        server_task.abort();
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn exposes_switchable_audio_and_webvtt_subtitle_renditions() -> TestResult {
    air_transcode::initialize()?;
    let fixtures = tempfile::tempdir()?;
    let fixture = fixtures.path().join("multi-track.mkv");
    generate_fixture(&fixture, FixtureKind::MultiTrack)?;
    let external_subtitle = fixtures.path().join("external-fr.srt");
    std::fs::write(
        &external_subtitle,
        "1\n00:00:00,600 --> 00:00:01,600\nBonjour externe\n\n2\n00:00:02,600 --> 00:00:03,600\nDeuxième externe\n",
    )?;
    let external_subtitle_url = url::Url::from_file_path(&external_subtitle)
        .map_err(|()| io::Error::other("failed to create external subtitle file URL"))?;
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
    let invalid_external = client
        .post(format!("{server_url}/v1/sessions"))
        .json(&json!({
            "source": {
                "url": format!("{origin_url}/media"),
                "headers": { "Authorization": "Bearer fixture" }
            },
            "subtitles": [{
                "source": { "url": "ftp://example.invalid/subtitle.srt" },
                "name": "Invalid"
            }]
        }))
        .send()
        .await?;
    assert_eq!(invalid_external.status(), StatusCode::BAD_REQUEST);
    let session: Value = client
        .post(format!("{server_url}/v1/sessions"))
        .json(&json!({
            "source": {
                "url": format!("{origin_url}/media"),
                "headers": { "Authorization": "Bearer fixture" }
            },
            "output": { "force_transcode": true, "max_width": 1920 },
            "subtitles": [{
                "source": { "url": external_subtitle_url },
                "name": "French External",
                "language": "fr"
            }]
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let id = json_string(&session, "id")?;
    let audio = session["tracks"]
        .as_array()
        .ok_or_else(|| io::Error::other("tracks are not an array"))?
        .iter()
        .filter(|track| track["kind"] == "audio")
        .filter_map(|track| track["index"].as_u64())
        .collect::<Vec<_>>();
    let subtitles = session["tracks"]
        .as_array()
        .ok_or_else(|| io::Error::other("tracks are not an array"))?
        .iter()
        .filter(|track| track["kind"] == "subtitle")
        .filter_map(|track| track["index"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(audio.len(), 2);
    assert_eq!(subtitles.len(), 3);

    let master = client
        .get(format!("{server_url}/v1/sessions/{id}/master.m3u8"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert_eq!(master.matches("TYPE=AUDIO").count(), 2);
    assert_eq!(master.matches("TYPE=SUBTITLES").count(), 3);
    assert!(master.contains("SUBTITLES=\"subtitles\""));
    for subtitle in &subtitles {
        let playlist = client
            .get(format!(
                "{server_url}/v1/sessions/{id}/subtitles/{subtitle}/playlist.m3u8"
            ))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(playlist.contains("segments/1"));
    }

    let first_audio = fetch_bytes(
        &client,
        format!(
            "{server_url}/v1/sessions/{id}/audio/{}/segments/1",
            audio[0]
        ),
    )
    .await?;
    let second_audio = fetch_bytes(
        &client,
        format!(
            "{server_url}/v1/sessions/{id}/audio/{}/segments/1",
            audio[1]
        ),
    )
    .await?;
    validate_media_segment(&first_audio)?;
    validate_media_segment(&second_audio)?;
    assert_ne!(media_data(&first_audio), media_data(&second_audio));

    let first_subtitle = fetch_bytes(
        &client,
        format!(
            "{server_url}/v1/sessions/{id}/subtitles/{}/segments/1",
            subtitles[0]
        ),
    )
    .await?;
    let second_subtitle = fetch_bytes(
        &client,
        format!(
            "{server_url}/v1/sessions/{id}/subtitles/{}/segments/1",
            subtitles[1]
        ),
    )
    .await?;
    assert!(first_subtitle.starts_with(b"WEBVTT"));
    assert!(second_subtitle.starts_with(b"WEBVTT"));
    assert_ne!(first_subtitle, second_subtitle);
    let subtitle_text = format!(
        "{} {}",
        String::from_utf8_lossy(&first_subtitle),
        String::from_utf8_lossy(&second_subtitle)
    );
    assert!(subtitle_text.contains("Hello") && subtitle_text.contains("Hola"));
    let external_text = fetch_bytes(
        &client,
        format!(
            "{server_url}/v1/sessions/{id}/subtitles/{}/segments/1",
            subtitles[2]
        ),
    )
    .await?;
    assert!(String::from_utf8_lossy(&external_text).contains("Bonjour externe"));

    let later_subtitles = futures_util::future::try_join_all(subtitles.iter().map(|track| {
        fetch_bytes(
            &client,
            format!("{server_url}/v1/sessions/{id}/subtitles/{track}/segments/2"),
        )
    }))
    .await?;
    let later_text = later_subtitles
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        later_text.contains("Second English") && later_text.contains("Segundo"),
        "unexpected later subtitles: {later_text}"
    );

    let cached_subtitle = fetch_bytes(
        &client,
        format!(
            "{server_url}/v1/sessions/{id}/subtitles/{}/segments/1",
            subtitles[0]
        ),
    )
    .await?;
    assert_eq!(cached_subtitle, first_subtitle);
    let metrics: Value = client
        .get(format!("{server_url}/v1/metrics"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        metrics["subtitle_segments"]
            .as_u64()
            .is_some_and(|value| value >= 4)
    );
    assert!(
        metrics["cache_hits"]
            .as_u64()
            .is_some_and(|value| value >= 1)
    );

    let invalid = client
        .get(format!(
            "{server_url}/v1/sessions/{id}/audio/9999/segments/1"
        ))
        .send()
        .await?;
    assert_eq!(invalid.status(), StatusCode::NOT_FOUND);

    origin_task.abort();
    server_task.abort();
    Ok(())
}

async fn create_session(
    client: &reqwest::Client,
    server_url: &str,
    origin_url: &str,
) -> TestResult<Value> {
    create_session_with_output(client, server_url, origin_url, json!({ "max_width": 1920 })).await
}

async fn create_session_with_output(
    client: &reqwest::Client,
    server_url: &str,
    origin_url: &str,
    output: Value,
) -> TestResult<Value> {
    Ok(client
        .post(format!("{server_url}/v1/sessions"))
        .json(&json!({
            "source": {
                "url": format!("{origin_url}/media"),
                "headers": { "Authorization": "Bearer fixture" }
            },
            "output": output
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
    H264LongGop,
    H265Aac,
    Av1Aac,
    Vp9Opus,
    MultiAac,
    MultiTrack,
}

fn generate_fixture(path: &Path, kind: FixtureKind) -> TestResult {
    if matches!(kind, FixtureKind::MultiAac) {
        return run_pipeline(
            &format!(
                "mp4mux name=mux ! filesink location={} audiotestsrc num-buffers=282 wave=sine freq=440 ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! queue ! mux. audiotestsrc num-buffers=282 wave=sine freq=880 ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! queue ! mux.",
                gst_launch_path(path)
            ),
            Duration::from_secs(20),
        );
    }
    if matches!(kind, FixtureKind::MultiTrack) {
        let english = path.with_extension("en.srt");
        let spanish = path.with_extension("es.srt");
        std::fs::write(
            &english,
            "1\n00:00:00,500 --> 00:00:01,500\nHello from English\n\n2\n00:00:02,500 --> 00:00:03,500\nSecond English cue\n",
        )?;
        std::fs::write(
            &spanish,
            "1\n00:00:00,500 --> 00:00:01,500\nHola desde Español\n\n2\n00:00:02,500 --> 00:00:03,500\nSegundo subtítulo\n",
        )?;
        return run_pipeline(
            &format!(
                "matroskamux name=mux min-index-interval=1000000000 ! filesink location={} videotestsrc num-buffers=180 pattern=smpte ! video/x-raw,format=I420,width=320,height=180,framerate=30/1 ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=30 bframes=0 byte-stream=false ! h264parse ! queue ! mux. audiotestsrc num-buffers=282 wave=sine freq=440 ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! taginject tags=\"language-code=en,title=English\" ! aacparse ! queue ! mux. audiotestsrc num-buffers=282 wave=sine freq=880 ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! taginject tags=\"language-code=es,title=Spanish\" ! aacparse ! queue ! mux. filesrc location={} ! subparse ! taginject tags=\"language-code=en,title=English\" ! queue ! mux. filesrc location={} ! subparse ! taginject tags=\"language-code=es,title=Spanish\" ! queue ! mux.",
                gst_launch_path(path),
                gst_launch_path(&english),
                gst_launch_path(&spanish)
            ),
            Duration::from_secs(30),
        );
    }
    let mux_and_video = match kind {
        FixtureKind::H264Aac => format!(
            "mp4mux name=mux ! filesink location={} videotestsrc num-buffers=180 pattern=smpte horizontal-speed=3 ! video/x-raw,format=I420,width=320,height=180,framerate=30/1 ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=30 bframes=0 byte-stream=false ! h264parse ! queue ! mux.",
            gst_launch_path(path)
        ),
        FixtureKind::H264LongGop => format!(
            "mp4mux name=mux ! filesink location={} videotestsrc num-buffers=300 pattern=smpte horizontal-speed=3 ! video/x-raw,format=I420,width=320,height=180,framerate=30/1 ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max=300 bframes=0 byte-stream=false ! h264parse ! queue ! mux.",
            gst_launch_path(path)
        ),
        FixtureKind::H265Aac => format!(
            "matroskamux name=mux ! filesink location={} videotestsrc num-buffers=90 pattern=smpte horizontal-speed=3 ! video/x-raw,format=I420,width=320,height=180,framerate=30/1 ! x265enc speed-preset=ultrafast tune=zerolatency key-int-max=30 ! h265parse ! queue ! mux.",
            gst_launch_path(path)
        ),
        FixtureKind::Av1Aac => format!(
            "matroskamux name=mux ! filesink location={} videotestsrc num-buffers=60 pattern=ball animation-mode=frames ! video/x-raw,format=I420,width=320,height=180,framerate=30/1 ! av1enc cpu-used=8 threads=4 keyframe-max-dist=30 ! av1parse ! queue ! mux.",
            gst_launch_path(path)
        ),
        FixtureKind::Vp9Opus => format!(
            "matroskamux name=mux ! filesink location={} videotestsrc num-buffers=180 pattern=ball animation-mode=frames ! video/x-raw,width=320,height=180,framerate=30/1 ! vp9enc deadline=1 keyframe-max-dist=30 ! queue ! mux.",
            gst_launch_path(path)
        ),
        FixtureKind::MultiAac => {
            return Err(io::Error::other("multi-audio fixture returned too late").into());
        }
        FixtureKind::MultiTrack => {
            return Err(io::Error::other("multi-track fixture returned too late").into());
        }
    };
    let audio = match kind {
        FixtureKind::H264Aac | FixtureKind::H264LongGop => {
            "audiotestsrc num-buffers=282 wave=white-noise ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! queue ! mux."
        }
        FixtureKind::H265Aac | FixtureKind::Av1Aac => {
            "audiotestsrc num-buffers=141 wave=white-noise ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! queue ! mux."
        }
        FixtureKind::Vp9Opus => {
            "audiotestsrc num-buffers=282 wave=ticks ! audio/x-raw,rate=48000,channels=2 ! opusenc ! queue ! mux."
        }
        FixtureKind::MultiAac => {
            return Err(io::Error::other("multi-audio fixture returned too late").into());
        }
        FixtureKind::MultiTrack => {
            return Err(io::Error::other("multi-track fixture returned too late").into());
        }
    };
    run_pipeline(&format!("{mux_and_video} {audio}"), Duration::from_secs(20))
}

fn gst_launch_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    format!("\"{}\"", normalized.replace('"', "\\\""))
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
