//! Reports which H.264 encoder and decoder this machine would use, and how
//! fast they are. Run with `RUST_LOG=debug` to see why GPU backends were
//! skipped, and `SCREENSHARE_FFMPEG_LOG=1` for FFmpeg's own messages.
//!
//!   cargo run --release -p screenshare-media --example codec_probe [WIDTHxHEIGHT]

use screenshare_media::video::{
    Codec, EncoderPrefs, PixelFormat, RawFrame, convert_frame, encoder_backend, open_decoder,
    open_encoder,
};
use std::time::{Duration, Instant};

const FRAMES: u64 = 120;

fn main() -> anyhow::Result<()> {
    tracing_subscriber_init();
    let (width, height) = std::env::args()
        .nth(1)
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1920, 1080));

    for hardware in [true, false] {
        println!(
            "\n== {} preferred, {width}x{height} ==",
            if hardware { "GPU" } else { "CPU" }
        );
        println!("encoder backend: {}", encoder_backend(hardware, Codec::H264));
        let mut encoder = open_encoder(EncoderPrefs {
            hardware,
            codec: Codec::H264,
            width,
            height,
            fps: 60,
            bitrate: 8_000_000,
        })?;
        let mut decoder = open_decoder(hardware, Codec::H264)?;

        let (mut convert, mut encode, mut decode) =
            (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        let (mut bytes, mut decoded) = (0usize, 0u64);
        for i in 0..FRAMES {
            let raw = moving_pattern(width, height, i);
            let t = Instant::now();
            let frame = convert_frame(&raw, width, height, encoder.input_layout());
            convert += t.elapsed();

            let t = Instant::now();
            let Some(packet) = encoder.encode(&frame)? else {
                continue;
            };
            encode += t.elapsed();
            bytes += packet.data.len();

            let t = Instant::now();
            if decoder.decode(&packet.data, packet.pts_us)?.is_some() {
                decoded += 1;
            }
            decode += t.elapsed();
        }
        let per = |d: Duration| d.as_secs_f64() * 1000.0 / FRAMES as f64;
        println!("encoder: {}  decoder: {}", encoder.name(), decoder.name());
        println!(
            "per frame: convert {:.2} ms, encode {:.2} ms, decode {:.2} ms",
            per(convert),
            per(encode),
            per(decode)
        );
        println!(
            "{decoded}/{FRAMES} frames decoded, {:.0} kbit/s at 60 fps",
            bytes as f64 * 8.0 * 60.0 / FRAMES as f64 / 1000.0
        );
    }
    Ok(())
}

/// A scrolling gradient with a moving box, so every frame differs.
fn moving_pattern(width: u32, height: u32, i: u64) -> RawFrame {
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![0u8; w * h * 4];
    let shift = i as usize * 8;
    let (bx, by) = ((i as usize * 13) % w.max(1), (i as usize * 7) % h.max(1));
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            let in_box = x.abs_diff(bx) < 80 && y.abs_diff(by) < 80;
            data[o] = if in_box {
                255
            } else {
                ((x + shift) % 256) as u8
            };
            data[o + 1] = ((y + shift / 2) % 256) as u8;
            data[o + 2] = if in_box { 0 } else { 128 };
            data[o + 3] = 255;
        }
    }
    RawFrame {
        width,
        height,
        stride: w * 4,
        format: PixelFormat::Bgra,
        data,
        pts_us: i * 16_667,
    }
}

fn tracing_subscriber_init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
