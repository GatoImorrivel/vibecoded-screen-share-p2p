//! Lists audio apps and captures two seconds of desktop audio, printing levels.

use screenshare_capture::{AudioTarget, list_audio_apps, start_audio};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    for app in list_audio_apps() {
        println!(
            "app: {} pid={:?} binary={:?}",
            app.name, app.pid, app.binary
        );
    }
    let exclude: Vec<String> = std::env::args().skip(1).collect();
    println!("excluding {exclude:?}");
    let capture = start_audio(AudioTarget::Desktop { exclude })?;
    let start = Instant::now();
    let (mut frames, mut peak) = (0, 0f32);
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok((_, frame)) = capture.frames.recv_timeout(Duration::from_millis(100)) {
            frames += 1;
            peak = frame.iter().fold(peak, |p, s| p.max(s.abs()));
        }
    }
    println!("captured {frames} frames of 20 ms, peak {peak:.3}");
    Ok(())
}
