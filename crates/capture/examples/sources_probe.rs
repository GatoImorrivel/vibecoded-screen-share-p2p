//! Lists capturable sources as the in-app picker sees them, with thumbnail sizes.
//!
//! `cargo run -p screenshare-capture --example sources_probe`

fn main() -> anyhow::Result<()> {
    println!("picker: {:?}", screenshare_capture::picker_kind());
    for source in screenshare_capture::list_sources()? {
        let thumb = screenshare_capture::thumbnail(&source, 320)
            .map(|t| format!("{}x{}", t.width, t.height))
            .unwrap_or_else(|| "none".into());
        println!(
            "{:?} id={} {}x{} pid={:?} app={:?} thumb={thumb} title={:?}",
            source.kind,
            source.id,
            source.width,
            source.height,
            source.pid,
            source.app_name,
            source.title
        );
    }
    Ok(())
}
