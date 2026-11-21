use std::path::Path;

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::gst_util::{add_chain, make};
use crate::ring::{Snapshot, Track};

/// Muxes a snapshot of the rolling buffer into an MP4 without re-encoding.
/// Returns the clip length.
pub fn write_mp4(snap: Snapshot, path: &Path) -> Result<gst::ClockTime> {
    let (Some(vcaps), Some(first), Some(last)) = (
        snap.video.caps.clone(),
        snap.video.packets.front(),
        snap.video.packets.back(),
    ) else {
        bail!("buffer is still empty");
    };
    let base = first.pts().unwrap();
    let length = last.pts().unwrap() - base;

    let pipeline = gst::Pipeline::with_name("save");
    let mux = make("mp4mux", &[("faststart", "true")])?;
    let sink = make(
        "filesink",
        &[("location", path.to_str().context("non-UTF-8 output path")?)],
    )?;
    pipeline.add_many([&mux, &sink])?;
    mux.link(&sink)?;

    let vsrc = appsrc(&vcaps);
    add_chain(&pipeline, &[vsrc.clone().upcast(), make("h264parse", &[])?])?;
    link_last(&pipeline, "h264parse", &mux)?;

    // Each enabled audio source becomes its own MP4 track, titled so the
    // built-in player can label its lanes.
    let mut audio = Vec::new();
    for (title, track) in &snap.aux {
        let (Some(acaps), false) = (&track.caps, track.packets.is_empty()) else {
            continue;
        };
        let acaps = acaps.clone();
        let parser = if acaps
            .structure(0)
            .is_some_and(|s| s.name() == "audio/x-opus")
        {
            "opusparse"
        } else {
            "aacparse"
        };
        let title = format!("title={title}");
        let asrc = appsrc(&acaps);
        let parser = make(parser, &[])?;
        let tags = make("taginject", &[("tags", &title)])?;
        pipeline.add_many([asrc.clone().upcast(), parser.clone(), tags.clone()])?;
        gst::Element::link_many([&asrc.clone().upcast(), &parser, &tags])?;
        tags.link(&mux)
            .context("mp4mux does not accept the audio track")?;
        audio.push((asrc, track));
    }

    pipeline.set_state(gst::State::Playing)?;
    push_track(&vsrc, &snap.video, base, true)?;
    for (asrc, track) in &audio {
        push_track(asrc, track, base, false)?;
    }

    let bus = pipeline.bus().unwrap();
    let result = loop {
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) else {
            break Err(anyhow::anyhow!("timed out writing clip"));
        };
        match msg.view() {
            gst::MessageView::Eos(_) => break Ok(length),
            gst::MessageView::Error(e) => break Err(anyhow::anyhow!("{}", e.error())),
            _ => {}
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn appsrc(caps: &gst::Caps) -> gst_app::AppSrc {
    gst_app::AppSrc::builder()
        .caps(caps)
        .format(gst::Format::Time)
        .is_live(false)
        .max_bytes(0)
        .build()
}

fn last_parser(pipeline: &gst::Pipeline, factory: &str) -> Option<gst::Element> {
    pipeline
        .children()
        .into_iter()
        .find(|e| e.factory().is_some_and(|f| f.name() == factory))
}

fn link_last(pipeline: &gst::Pipeline, factory: &str, mux: &gst::Element) -> Result<()> {
    let parser = last_parser(pipeline, factory).context("parser missing")?;
    parser
        .link(mux)
        .with_context(|| format!("mp4mux does not accept {factory} output"))?;
    Ok(())
}

fn push_track(
    src: &gst_app::AppSrc,
    track: &Track,
    base: gst::ClockTime,
    video: bool,
) -> Result<()> {
    for buf in &track.packets {
        let pts = buf.pts().unwrap();
        if pts < base {
            continue;
        }
        let mut out = buf.copy();
        {
            let out = out.make_mut();
            out.set_pts(pts - base);
            let dts = buf.dts().or(if video { Some(pts) } else { None });
            out.set_dts(dts.map(|d| d.saturating_sub(base)));
        }
        src.push_buffer(out).context("failed to queue packet")?;
    }
    src.end_of_stream().context("failed to end stream")?;
    Ok(())
}
