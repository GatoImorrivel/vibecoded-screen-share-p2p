//! Audio capture with WASAPI process loopback (Windows 10 2004+).
//!
//! Whole-desktop capture uses a single "everything except our process tree"
//! client. When an excluded app (e.g. Discord) is playing, it switches to one
//! "include" client per other playing process and mixes them.

use crate::mixer::{SourceMixer, SourceWriter};
use crate::{AudioApp, AudioCapture, AudioTarget};
use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use wasapi::{
    AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta,
};

fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let file = path.rsplit(['\\', '/']).next()?.to_string();
        Some(file.trim_end_matches(".exe").to_string())
    }
}

/// Processes with an audio session on the default render device.
pub(crate) fn list_apps() -> Result<Vec<AudioApp>> {
    let _ = initialize_mta();
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_default_device(Direction::Render)?;
    let manager = device.get_iaudiosessionmanager()?;
    let sessions = manager.get_audiosessionenumerator()?;
    let mut apps = Vec::new();
    for i in 0..sessions.get_count()? {
        let control = sessions.get_session(i)?;
        let pid = control.get_process_id()?;
        if pid == 0 {
            continue;
        }
        let name = process_name(pid).unwrap_or_else(|| format!("Process {pid}"));
        apps.push(AudioApp {
            binary: Some(name.clone()),
            name,
            pid: Some(pid),
        });
    }
    Ok(apps)
}

struct ClientHandle {
    running: Arc<AtomicBool>,
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn spawn_client(pid: u32, include_tree: bool, writer: SourceWriter) -> ClientHandle {
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let _ = std::thread::Builder::new()
        .name(format!("wasapi-loopback-{pid}"))
        .spawn(move || {
            if let Err(err) = capture_loop(pid, include_tree, &writer, &flag) {
                tracing::warn!("loopback capture for pid {pid} failed: {err:#}");
            }
        });
    ClientHandle { running }
}

fn capture_loop(
    pid: u32,
    include_tree: bool,
    writer: &SourceWriter,
    running: &AtomicBool,
) -> Result<()> {
    let _ = initialize_mta();
    let format = WaveFormat::new(
        32,
        32,
        SampleType::Float,
        screenshare_media::SAMPLE_RATE as usize,
        2,
        None,
    );
    let mut client = AudioClient::new_application_loopback_client(pid, include_tree)
        .context("creating loopback client")?;
    client.initialize_client(
        &format,
        Direction::Capture,
        StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 0,
        },
        None,
    )?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    let mut queue: VecDeque<u8> = VecDeque::new();
    while running.load(Ordering::Relaxed) {
        if event.wait_for_event(200).is_err() {
            continue;
        }
        capture.read_from_device_to_deque(&mut queue)?;
        let usable = queue.len() - queue.len() % 4;
        let bytes: Vec<u8> = queue.drain(..usable).collect();
        let samples: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        writer.push(&samples);
    }
    let _ = client.stop_stream();
    Ok(())
}

pub(crate) fn start(target: AudioTarget) -> Result<AudioCapture> {
    let (mixer, frames) = SourceMixer::start();
    let own_pid = std::process::id();
    let running = Arc::new(AtomicBool::new(true));

    match target {
        AudioTarget::App(app) => {
            let pid = app
                .pid
                .ok_or_else(|| anyhow!("{} has no process id", app.name))?;
            let client = spawn_client(pid, true, mixer.add_source());
            let flag = running.clone();
            let m = mixer.clone();
            return Ok(AudioCapture::new(
                frames,
                Box::new(move || {
                    flag.store(false, Ordering::Relaxed);
                    drop(client);
                    m.stop();
                }),
            ));
        }
        AudioTarget::Desktop { exclude } => {
            let flag = running.clone();
            let m = mixer.clone();
            std::thread::Builder::new()
                .name("wasapi-supervisor".into())
                .spawn(move || supervise(m, exclude, own_pid, flag))?;
        }
    }

    Ok(AudioCapture::new(
        frames,
        Box::new(move || {
            running.store(false, Ordering::Relaxed);
            mixer.stop();
        }),
    ))
}

/// Keeps the set of loopback clients matching the exclusion list as apps come and go.
fn supervise(mixer: SourceMixer, exclude: Vec<String>, own_pid: u32, running: Arc<AtomicBool>) {
    let mut everything: Option<ClientHandle> = None;
    let mut per_process: HashMap<u32, ClientHandle> = HashMap::new();
    while running.load(Ordering::Relaxed) {
        let apps = list_apps().unwrap_or_default();
        let excluded_playing = apps.iter().any(|a| exclude.iter().any(|e| a.matches(e)));
        if !excluded_playing {
            per_process.clear();
            if everything.is_none() {
                everything = Some(spawn_client(own_pid, false, mixer.add_source()));
            }
        } else {
            everything = None;
            let wanted: Vec<u32> = apps
                .iter()
                .filter(|a| a.pid != Some(own_pid) && !exclude.iter().any(|e| a.matches(e)))
                .filter_map(|a| a.pid)
                .collect();
            per_process.retain(|pid, _| wanted.contains(pid));
            for pid in wanted {
                per_process
                    .entry(pid)
                    .or_insert_with(|| spawn_client(pid, true, mixer.add_source()));
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
