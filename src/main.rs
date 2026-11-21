mod capture;
mod clips;
mod config;
mod daemon;
mod encoder;
mod gst_util;
mod platform;
mod player;
mod ring;
mod save;
mod sysinfo;
mod toast;
mod ui;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

use capture::AudioSource;
use config::Config;
use encoder::Encoder;
use platform::ipc;

const USAGE: &str = "\
clip-recorder: keeps the last few seconds of your screen in RAM, saves them on a hotkey

USAGE:
    clip-recorder [run] [OPTIONS]   start recording (tray icon, Alt+S to save)
    clip-recorder save              save a clip from the running recorder
    clip-recorder pause|resume      pause or resume the buffer (pausing frees its RAM)
    clip-recorder reselect          choose the screen to record again
    clip-recorder status            print the recorder's state
    clip-recorder library           open the clips window
    clip-recorder update [--check]  install the latest release (or only check for one)
    clip-recorder --version         print the version
    clip-recorder quit              stop the running recorder

OPTIONS (override ~/.config/clip-recorder/config for this run):
    -d, --duration <SECS>     seconds kept in the buffer          [default: 60]
    -f, --fps <N>             frame rate cap                      [default: 60]
    -b, --bitrate <KBPS>      video bitrate, overrides the quality preset
    -e, --encoder <NAME>      auto, va, vaapi, nvenc, x264        [default: auto]
    -a, --audio <MODE>        auto (per-app tracks), pipewire|pulse (whole desktop, one track), none
                              [default: auto]
    -m, --mic <SOURCE>        microphone, its own audio track     [default: auto]
        --mic-device <NAME>   microphone to record, from the library's list [default: system default]
    -o, --output <DIR>        where clips go                      [default: ~/Videos/Clips]
    -k, --hotkey <COMBO>      global save hotkey                  [default: alt+s]
        --no-hotkey           only save via `clip-recorder save` or SIGUSR1
        --headless            no tray or toasts
        --background          start quietly in the tray (what autostart uses); otherwise the
                              library opens, and onboarding shows on the very first start
        --reselect            show the screen picker again
        --test-source         record a test pattern and tone instead of the screen
    -h, --help                print this help
";

enum Cmd {
    Run(Config, daemon::Flags),
    Client(&'static str),
    Window(&'static str),
    Update { check: bool, notify: bool },
    Version,
    Help,
}

fn parse_args() -> Result<Cmd> {
    use lexopt::prelude::*;
    let mut cfg = Config::load();
    let mut flags = daemon::Flags {
        kbps: None,
        hotkey: true,
        reselect: false,
        background: false,
        test_source: false,
        ui: true,
    };
    let mut parser = lexopt::Parser::from_env();
    let mut first = true;
    while let Some(arg) = parser.next()? {
        match arg {
            Value(cmd) if first => {
                let cmd = cmd.string()?;
                match cmd.as_str() {
                    "run" => {}
                    "save" => return Ok(Cmd::Client("save")),
                    "pause" => return Ok(Cmd::Client("pause")),
                    "resume" => return Ok(Cmd::Client("resume")),
                    "status" => return Ok(Cmd::Client("status")),
                    "reselect" => return Ok(Cmd::Client("reselect")),
                    "quit" => return Ok(Cmd::Client("quit")),
                    "library" => return Ok(Cmd::Window("library")),
                    "update" => {
                        let (mut check, mut notify) = (false, false);
                        while let Some(a) = parser.next()? {
                            match a {
                                Long("check") => check = true,
                                Long("notify") => notify = true,
                                _ => return Err(a.unexpected().into()),
                            }
                        }
                        return Ok(Cmd::Update { check, notify });
                    }
                    "osd" => return Ok(Cmd::Window("osd")),
                    _ => bail!("unknown command '{cmd}'"),
                }
            }
            Short('d') | Long("duration") => cfg.duration = parser.value()?.parse()?,
            Short('f') | Long("fps") => cfg.fps = parser.value()?.parse()?,
            Short('b') | Long("bitrate") => flags.kbps = Some(parser.value()?.parse()?),
            Short('e') | Long("encoder") => {
                cfg.encoder = Encoder::parse(&parser.value()?.string()?)?
            }
            Short('a') | Long("audio") => {
                cfg.audio = AudioSource::parse(&parser.value()?.string()?)?
            }
            Short('m') | Long("mic") => {
                cfg.mic = AudioSource::parse(&parser.value()?.string()?)?
            }
            Long("mic-device") => cfg.mic_device = parser.value()?.string()?,
            Short('o') | Long("output") => cfg.output = parser.value()?.into(),
            Short('k') | Long("hotkey") => cfg.hotkey = parser.value()?.string()?,
            Long("no-hotkey") => flags.hotkey = false,
            Long("headless") | Long("no-notify") => flags.ui = false,
            Long("reselect") => flags.reselect = true,
            Long("background") => flags.background = true,
            Long("test-source") => flags.test_source = true,
            Short('h') | Long("help") => return Ok(Cmd::Help),
            Short('V') | Long("version") => return Ok(Cmd::Version),
            _ => return Err(arg.unexpected().into()),
        }
        first = false;
    }
    if cfg.duration == 0 || cfg.fps == 0 || flags.kbps == Some(0) {
        bail!("duration, fps and bitrate must be greater than 0");
    }
    Ok(Cmd::Run(cfg, flags))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into())
}

/// `notify` (from the tray) reports the outcome as a desktop notification too.
fn run_update(check: bool, notify: bool) -> Result<()> {
    let say = |title: &str, body: &str| {
        println!("{body}");
        if notify {
            daemon::notify(title, body);
        }
    };
    let result = (|| {
        let Some(rel) = update::check()? else {
            say(
                "Clip Recorder",
                &format!("clip-recorder {} is up to date", env!("CARGO_PKG_VERSION")),
            );
            return Ok(());
        };
        if check {
            println!(
                "update available: v{} (run `clip-recorder update`)",
                rel.version
            );
            return Ok(());
        }
        println!("installing v{}...", rel.version);
        let path = update::apply(&rel)?;
        say(
            "Clip Recorder updated",
            &format!("Updated to v{} ({})", rel.version, path.display()),
        );
        update::restart(&path);
        Ok(())
    })();
    if let (Err(e), true) = (&result, notify) {
        daemon::notify("Couldn't update Clip Recorder", &format!("{e:#}"));
    }
    result
}

fn main() -> ExitCode {
    let result = match parse_args() {
        Ok(Cmd::Help) => {
            print!("{USAGE}");
            Ok(())
        }
        Ok(Cmd::Client(cmd)) => ipc::request(cmd).map(|out| {
            if !out.is_empty() {
                println!("{}", out.trim_end());
            }
        }),
        Ok(Cmd::Version) => {
            println!("clip-recorder {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Ok(Cmd::Update { check, notify }) => run_update(check, notify),
        Ok(Cmd::Window(w)) => match w {
            "osd" => toast::run(),
            _ => ui::library(),
        },
        // Started again while recording: bring up the library instead of failing,
        // so the app menu entry always does something.
        Ok(Cmd::Run(_, flags)) if ipc::status().is_ok() => {
            if flags.background {
                Ok(())
            } else {
                ui::library()
            }
        }
        Ok(Cmd::Run(cfg, flags)) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(Into::into)
            .and_then(|rt| rt.block_on(daemon::run(cfg, flags))),
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
