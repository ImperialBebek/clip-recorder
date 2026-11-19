mod capture;
mod encoder;
mod gst_util;
mod platform;
mod ring;
mod save;

use platform::{hotkey, ipc, portal};

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

use capture::{AudioSource, BusEvent, Recorder, Settings, VideoSource};
use encoder::Encoder;
use ring::Ring;

const USAGE: &str = "\
clip-recorder: keeps the last few seconds of your screen in RAM, saves them on a hotkey

USAGE:
    clip-recorder [run] [OPTIONS]   start recording into the rolling buffer
    clip-recorder save              save a clip from the running recorder

OPTIONS:
    -d, --duration <SECS>     seconds kept in the buffer          [default: 30]
    -f, --fps <N>             frame rate cap                      [default: 60]
    -b, --bitrate <KBPS>      video bitrate in kbit/s             [default: 20000]
    -e, --encoder <NAME>      auto, va, vaapi, nvenc, x264        [default: auto]
    -a, --audio <SOURCE>      auto, pipewire, pulse, none         [default: auto]
    -o, --output <DIR>        where clips go                      [default: ~/Videos/Clips]
    -k, --hotkey <COMBO>      global save hotkey                  [default: alt+s]
        --no-hotkey           only save via `clip-recorder save` or SIGUSR1
        --no-notify           no desktop notification on save
        --reselect            show the screen picker again
        --test-source         record a test pattern instead of the screen
    -h, --help                print this help
";

struct Options {
    duration: u32,
    fps: u32,
    kbps: u32,
    encoder: Option<Encoder>,
    audio: AudioSource,
    output: PathBuf,
    hotkey: Option<String>,
    notify: bool,
    reselect: bool,
    test_source: bool,
}

enum Command {
    Run(Options),
    Save,
    Help,
}

fn parse_args() -> Result<Command> {
    use lexopt::prelude::*;
    let mut o = Options {
        duration: 30,
        fps: 60,
        kbps: 20_000,
        encoder: None,
        audio: AudioSource::Auto,
        output: home().join("Videos").join("Clips"),
        hotkey: Some("alt+s".into()),
        notify: true,
        reselect: false,
        test_source: false,
    };
    let mut parser = lexopt::Parser::from_env();
    let mut first = true;
    while let Some(arg) = parser.next()? {
        match arg {
            Value(cmd) if first && cmd == "save" => return Ok(Command::Save),
            Value(cmd) if first && cmd == "run" => {}
            Short('d') | Long("duration") => o.duration = parser.value()?.parse()?,
            Short('f') | Long("fps") => o.fps = parser.value()?.parse()?,
            Short('b') | Long("bitrate") => o.kbps = parser.value()?.parse()?,
            Short('e') | Long("encoder") => o.encoder = Encoder::parse(&parser.value()?.string()?)?,
            Short('a') | Long("audio") => o.audio = AudioSource::parse(&parser.value()?.string()?)?,
            Short('o') | Long("output") => o.output = parser.value()?.into(),
            Short('k') | Long("hotkey") => o.hotkey = Some(parser.value()?.string()?),
            Long("no-hotkey") => o.hotkey = None,
            Long("no-notify") => o.notify = false,
            Long("reselect") => o.reselect = true,
            Long("test-source") => o.test_source = true,
            Short('h') | Long("help") => return Ok(Command::Help),
            _ => return Err(arg.unexpected().into()),
        }
        first = false;
    }
    if o.duration == 0 || o.fps == 0 || o.kbps == 0 {
        bail!("duration, fps and bitrate must be greater than 0");
    }
    Ok(Command::Run(o))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let result = match parse_args() {
        Ok(Command::Help) => {
            print!("{USAGE}");
            Ok(())
        }
        Ok(Command::Save) => ipc::send_save().map(|path| println!("{path}")),
        Ok(Command::Run(o)) => run(o).await,
        Err(e) => Err(e).context("try --help"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(o: Options) -> Result<()> {
    gst::init()?;
    std::fs::create_dir_all(&o.output)
        .with_context(|| format!("cannot create {}", o.output.display()))?;

    let (save_tx, mut save_rx) = mpsc::unbounded_channel::<Option<ipc::Reply>>();
    ipc::serve(save_tx.clone())?;

    // Held for the whole run: dropping it closes the portal session.
    let portal = if o.test_source {
        None
    } else {
        Some(portal::open(o.reselect).await?)
    };
    let source = match &portal {
        Some(p) => VideoSource::Screen(platform::Screen {
            fd: p.fd.as_raw_fd(),
            node: p.node,
        }),
        None => VideoSource::Test,
    };

    let ring = Arc::new(Mutex::new(Ring::new(gst::ClockTime::from_seconds(
        o.duration.into(),
    ))));
    let (bus_tx, mut bus_rx) = mpsc::unbounded_channel();
    let settings = Settings {
        fps: o.fps,
        kbps: o.kbps,
        encoder: o.encoder,
        audio: o.audio,
    };
    let mut recorder = Recorder::start(&source, &settings, ring.clone(), move |ev| {
        let _ = bus_tx.send(ev);
    })?;

    let hotkey_label = match &o.hotkey {
        Some(combo) => {
            let tx = save_tx.clone();
            let n = hotkey::spawn(hotkey::Hotkey::parse(combo)?, move || {
                let _ = tx.send(None);
            });
            if n == 0 {
                eprintln!(
                    "hotkey: no readable keyboard in /dev/input; add yourself to the `input` group \
                     (sudo usermod -aG input $USER, then log in again) or bind `clip-recorder save` \
                     to a shortcut in your desktop"
                );
                "clip-recorder save".to_owned()
            } else {
                combo.to_owned()
            }
        }
        None => "clip-recorder save".to_owned(),
    };

    eprintln!(
        "recording: {}s buffer, {} fps cap, {} kbit/s, encoder {}, audio {}. press {} to save a clip to {}",
        o.duration,
        o.fps,
        o.kbps,
        recorder.encoder.name(),
        recorder.audio_name.unwrap_or("off"),
        hotkey_label,
        o.output.display()
    );

    let mut usr1 = signal(SignalKind::user_defined1())?;
    let mut term = signal(SignalKind::terminate())?;
    let saving = Arc::new(AtomicBool::new(false));

    let outcome = loop {
        tokio::select! {
            Some(reply) = save_rx.recv() => {
                start_save(&ring, &o, &saving, reply);
            }
            _ = usr1.recv() => start_save(&ring, &o, &saving, None),
            Some(ev) = bus_rx.recv() => match ev {
                BusEvent::VideoFailed(e) => break Err(anyhow::anyhow!("video capture stopped: {e}")),
                BusEvent::AudioFailed(e) => {
                    eprintln!("audio capture stopped ({e}), continuing with video only");
                    recorder.stop_audio();
                }
            },
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = term.recv() => break Ok(()),
        }
    };
    recorder.stop();
    drop(portal);
    let _ = std::fs::remove_file(ipc::socket_path());
    outcome
}

fn start_save(
    ring: &Arc<Mutex<Ring>>,
    o: &Options,
    saving: &Arc<AtomicBool>,
    reply: Option<ipc::Reply>,
) {
    if saving.swap(true, Ordering::SeqCst) {
        if let Some(r) = reply {
            let _ = r.send(Err("a clip is already being saved".into()));
        }
        return;
    }
    let snap = ring.lock().unwrap().snapshot();
    let name = gst::glib::DateTime::now_local()
        .and_then(|t| t.format("clip_%Y-%m-%d_%H-%M-%S.mp4"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "clip.mp4".into());
    let path = o.output.join(name);
    let notify = o.notify;
    let saving = saving.clone();
    tokio::task::spawn_blocking(move || {
        let result = save::write_mp4(snap, &path);
        saving.store(false, Ordering::SeqCst);
        let msg = match result {
            Ok(len) => {
                eprintln!(
                    "saved {:.1}s clip: {}",
                    len.mseconds() as f64 / 1000.0,
                    path.display()
                );
                if notify {
                    let _ = std::process::Command::new("notify-send")
                        .args([
                            "-a",
                            "clip-recorder",
                            "-i",
                            "video-x-generic",
                            "-t",
                            "3000",
                            "Clip saved",
                        ])
                        .arg(path.display().to_string())
                        .spawn();
                }
                Ok(path.display().to_string())
            }
            Err(e) => {
                eprintln!("save failed: {e:#}");
                Err(format!("{e:#}"))
            }
        };
        if let Some(r) = reply {
            let _ = r.send(msg);
        }
    });
}
