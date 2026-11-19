use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Creates an element and sets the given properties, skipping any the
/// installed plugin version does not have so one spec works across versions.
pub fn make(factory: &str, props: &[(&str, &str)]) -> Result<gst::Element> {
    let el = gst::ElementFactory::make(factory)
        .build()
        .with_context(|| format!("GStreamer element '{factory}' is not installed"))?;
    for (name, value) in props {
        let Some(pspec) = el.find_property(name) else {
            continue;
        };
        let v = glib_value(&pspec, value)
            .with_context(|| format!("bad value '{value}' for {factory}.{name}"))?;
        el.set_property(name, v);
    }
    Ok(el)
}

fn glib_value(pspec: &gst::glib::ParamSpec, value: &str) -> Result<gst::glib::Value> {
    gst::glib::Value::deserialize_with_pspec(value, pspec).map_err(|e| anyhow!("{e}"))
}

pub fn capsfilter(caps: &str) -> Result<gst::Element> {
    let caps: gst::Caps = caps.parse().map_err(|_| anyhow!("invalid caps '{caps}'"))?;
    Ok(gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()?)
}

pub fn has(factory: &str) -> bool {
    gst::ElementFactory::find(factory).is_some()
}

pub fn add_chain(pipeline: &gst::Pipeline, chain: &[gst::Element]) -> Result<()> {
    pipeline.add_many(chain)?;
    gst::Element::link_many(chain).context("failed to link pipeline elements")?;
    Ok(())
}
