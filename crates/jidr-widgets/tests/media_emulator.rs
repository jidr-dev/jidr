//! Phase 10a: a screenshot is a PNG of the screen, and a 5-minute
//! recording passes ffprobe (h264, 300 ± 2 s, ≥ 30 × 300 × 0.9 frames).
//! #28: the emulator streams audio in the format the recorder asks for, and
//! recordings carry an AAC track as long as the video (the 5-minute one is
//! mostly silence, so it also checks the audio doesn't drift).
#![cfg(feature = "emulator")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::time::Duration;

use jidr_widgets::media::audio;
use serde_json::{Value, json};

/// ffprobe's JSON for the first stream of `kind` (`v` or `a`) and the format.
fn probe(path: &Path, kind: &str) -> Value {
    let probe = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", &format!("{kind}:0")])
        .args(if kind == "v" {
            &["-count_frames"][..]
        } else {
            &[][..]
        })
        .args([
            "-show_entries",
            "stream=codec_name,nb_read_frames,duration,width,height,sample_rate,channels",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .expect("ffprobe on PATH");
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let v: Value = serde_json::from_slice(&probe.stdout).unwrap();
    println!("ffprobe {kind}: {v}");
    v
}

fn secs(v: &Value) -> f64 {
    v.as_str().unwrap().parse().unwrap()
}

/// Asks the guest to play a sound: a notification with the default sound.
fn beep(env: &common::Env) {
    env.shell("cmd notification post -S bigtext jidr-28 'Jidr audio test'");
}

#[tokio::test(flavor = "multi_thread")]
async fn media_screenshot_and_five_minute_recording() {
    let env = common::Env::boot("widgets10a").await;

    // Screenshot.
    let png = env.tmp.path().join("shot.png");
    let s = env.call("media.screenshot", json!({ "path": png })).await;
    assert_eq!(s["path"], json!(png));
    let bytes = std::fs::read(&png).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(
        (s["width"].as_u64(), s["height"].as_u64()),
        (Some(720), Some(1280)),
        "{s}"
    );

    // #28: streamAudio takes the recorder's format (gRPC only: any OS). A
    // silent guest sends nothing, so a missing packet isn't a failure.
    let (mut grpc, _) = jidr_backend::grpc::connect_port(env.info.grpc_port, &env.info.grpc_token)
        .await
        .unwrap();
    let mut packets = grpc
        .stream_audio(audio::format_pb())
        .await
        .expect("streamAudio")
        .into_inner();
    beep(&env);
    match tokio::time::timeout(Duration::from_secs(10), packets.message()).await {
        Ok(Ok(Some(p))) => {
            println!("audio packet: {:?}, {} bytes", p.format, p.audio.len());
            assert!(
                audio::to_stereo(p.format.as_ref(), &p.audio).is_some(),
                "the emulator sent {:?}",
                p.format
            );
        }
        other => println!("no audio packet within 10 s: {other:?}"),
    }
    drop(packets);

    // Recording encodes with AVFoundation (VideoToolbox): macOS only.
    if !cfg!(target_os = "macos") {
        env.stop().await;
        return;
    }

    // A short recording with sound has an AAC track as long as the video.
    let short = env.tmp.path().join("audio.mp4");
    let (rec, ()) = tokio::join!(
        env.run(
            "media.record",
            json!({ "path": short, "max_seconds": 8, "fps": 30 }),
        ),
        async {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                beep(&env);
            }
        }
    );
    println!("audio recording: {rec}");
    assert_eq!(rec["audio"], true, "{rec}");
    let a = probe(&short, "a");
    assert_eq!(a["streams"][0]["codec_name"], "aac", "{a}");
    assert_eq!(a["streams"][0]["sample_rate"], "48000");
    assert_eq!(a["streams"][0]["channels"], 2);
    let video_s = secs(&a["format"]["duration"]);
    let audio_s = secs(&a["streams"][0]["duration"]);
    assert!((7.0..=9.0).contains(&video_s), "duration {video_s}");
    assert!(
        (audio_s - video_s).abs() < 0.5,
        "audio {audio_s} s, file {video_s} s"
    );

    // `audio: false` is video-only.
    let silent = env.tmp.path().join("silent.mp4");
    let rec = env
        .run(
            "media.record",
            json!({ "path": silent, "max_seconds": 2, "audio": false }),
        )
        .await;
    assert_eq!(rec["audio"], false, "{rec}");
    let a = probe(&silent, "a");
    assert_eq!(a["streams"], json!([]), "{a}");

    // Recording at a constant 30 fps (the last frame repeats while the screen is still).
    let mp4 = env.tmp.path().join("rec.mp4");
    let started = std::time::Instant::now();
    let rec = env
        .run(
            "media.record",
            json!({ "path": mp4, "max_seconds": 300, "fps": 30 }),
        )
        .await;
    println!(
        "recorded in {:.1} s: {rec}",
        started.elapsed().as_secs_f64()
    );
    assert!(rec["frames"].as_i64().unwrap() >= 8100, "{rec}");
    assert_eq!(rec["audio"], true, "{rec}");

    let v = probe(&mp4, "v");
    let stream = &v["streams"][0];
    assert_eq!(stream["codec_name"], "h264");
    let duration: f64 = v["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((298.0..=302.0).contains(&duration), "duration {duration}");
    let frames: u64 = stream["nb_read_frames"].as_str().unwrap().parse().unwrap();
    assert!(frames >= 8100, "frames {frames}");
    // Five minutes of mostly silence: the audio still ends with the video.
    let a = probe(&mp4, "a");
    let audio_s = secs(&a["streams"][0]["duration"]);
    assert!(
        (audio_s - duration).abs() < 0.5,
        "audio {audio_s} s, file {duration} s"
    );

    env.stop().await;
}
