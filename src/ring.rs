use std::collections::VecDeque;

use gstreamer as gst;

/// Encoded packets of one stream, oldest first.
#[derive(Default, Clone)]
pub struct Track {
    pub caps: Option<gst::Caps>,
    pub packets: VecDeque<gst::Buffer>,
}

impl Track {
    fn bytes(&self) -> usize {
        self.packets.iter().map(|b| b.size()).sum()
    }
}

/// Rolling buffer of already-encoded video and audio packets.
///
/// The video track always starts on a keyframe, so a snapshot can be muxed
/// without re-encoding. It keeps the shortest keyframe-aligned window that
/// still covers `window`.
pub struct Ring {
    window: gst::ClockTime,
    pub video: Track,
    /// Extra audio tracks (Game, Voice, Other, Mic, …) with their titles.
    pub aux: Vec<(String, Track)>,
}

pub struct Snapshot {
    pub video: Track,
    pub aux: Vec<(String, Track)>,
}

fn is_keyframe(buf: &gst::BufferRef) -> bool {
    !buf.flags().contains(gst::BufferFlags::DELTA_UNIT)
}

impl Ring {
    pub fn new(window: gst::ClockTime) -> Self {
        Self {
            window,
            video: Track::default(),
            aux: Vec::new(),
        }
    }

    pub fn push_video(&mut self, caps: Option<gst::Caps>, buf: gst::Buffer) {
        if buf.pts().is_none() {
            return;
        }
        if self.video.packets.is_empty() && !is_keyframe(&buf) {
            return;
        }
        if caps.is_some() {
            self.video.caps = caps;
        }
        let latest = buf.pts().unwrap();
        self.video.packets.push_back(buf);
        self.trim(latest);
    }

    /// Replaces the aux track set (called when the recorder starts; the old
    /// tracks' buffered packets are dropped).
    pub fn set_aux(&mut self, titles: &[String]) {
        self.aux = titles
            .iter()
            .map(|t| (t.clone(), Track::default()))
            .collect();
    }

    /// Pushes an encoded buffer to aux track `index`.
    pub fn push_aux(&mut self, index: usize, caps: Option<gst::Caps>, buf: gst::Buffer) {
        let Some((_, track)) = self.aux.get_mut(index) else {
            return;
        };
        Self::push_aux_track(track, self.window, self.video.packets.is_empty(), caps, buf);
    }

    /// An encoded stream trimmed on its own clock until video shows up, then
    /// trimmed to the video window by `trim`.
    fn push_aux_track(
        track: &mut Track,
        window: gst::ClockTime,
        video_empty: bool,
        caps: Option<gst::Caps>,
        buf: gst::Buffer,
    ) {
        if buf.pts().is_none() {
            return;
        }
        if caps.is_some() {
            track.caps = caps;
        }
        track.packets.push_back(buf);
        if video_empty {
            let latest = track.packets.back().unwrap().pts().unwrap();
            let cutoff = latest.saturating_sub(window);
            while track
                .packets
                .front()
                .is_some_and(|b| b.pts().unwrap() < cutoff)
            {
                track.packets.pop_front();
            }
        }
    }

    fn trim(&mut self, latest: gst::ClockTime) {
        let cutoff = latest.saturating_sub(self.window);
        // Drop whole GOPs from the front while the next keyframe still covers the window.
        loop {
            let next_key = self
                .video
                .packets
                .iter()
                .skip(1)
                .position(|b| is_keyframe(b))
                .map(|i| i + 1);
            match next_key {
                Some(i) if self.video.packets[i].pts().unwrap() <= cutoff => {
                    self.video.packets.drain(..i);
                }
                _ => break,
            }
        }
        let start = self
            .video
            .packets
            .front()
            .and_then(|b| b.pts())
            .unwrap_or(cutoff);
        for (_, track) in &mut self.aux {
            while track
                .packets
                .front()
                .is_some_and(|b| b.pts().unwrap() < start)
            {
                track.packets.pop_front();
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            video: self.video.clone(),
            aux: self.aux.clone(),
        }
    }

    pub fn bytes(&self) -> usize {
        self.video.bytes() + self.aux.iter().map(|(_, t)| t.bytes()).sum::<usize>()
    }

    /// Takes effect on the next GOP boundary; no restart needed.
    pub fn set_window(&mut self, window: gst::ClockTime) {
        self.window = window;
        if let Some(latest) = self.video.packets.back().and_then(|b| b.pts()) {
            self.trim(latest);
        }
    }

    /// Length of video currently held.
    pub fn buffered(&self) -> gst::ClockTime {
        let first = self.video.packets.front().and_then(|b| b.pts());
        let last = self.video.packets.back().and_then(|b| b.pts());
        match (first, last) {
            (Some(a), Some(b)) => b.saturating_sub(a),
            _ => gst::ClockTime::ZERO,
        }
    }

    pub fn clear(&mut self) {
        self.video.packets.clear();
        for (_, t) in &mut self.aux {
            t.packets.clear();
        }
    }

    /// "1080p60" from the negotiated caps.
    pub fn video_label(&self, fps: u32) -> Option<String> {
        let caps = self.video.caps.as_ref()?;
        let h = caps.structure(0)?.get::<i32>("height").ok()?;
        Some(format!("{h}p{fps}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(pts_ms: u64, key: bool) -> gst::Buffer {
        let mut b = gst::Buffer::with_size(10).unwrap();
        {
            let b = b.get_mut().unwrap();
            b.set_pts(gst::ClockTime::from_mseconds(pts_ms));
            if !key {
                b.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        b
    }

    #[test]
    fn keeps_keyframe_aligned_window() {
        gst::init().unwrap();
        let mut ring = Ring::new(gst::ClockTime::from_seconds(2));
        ring.set_aux(&["Game".into(), "Mic".into()]);
        // 10 fps, keyframe every second, 5 seconds of video.
        for i in 0..50u64 {
            ring.push_video(None, buf(i * 100, i % 10 == 0));
            ring.push_aux(0, None, buf(i * 100, true));
            ring.push_aux(1, None, buf(i * 100, true));
        }
        let first = ring.video.packets.front().unwrap();
        assert!(is_keyframe(first));
        // latest = 4.9s, cutoff = 2.9s -> must start at the 2s keyframe.
        assert_eq!(first.pts(), Some(gst::ClockTime::from_seconds(2)));
        assert_eq!(ring.video.packets.len(), 30);
        assert_eq!(ring.aux[0].1.packets.front().unwrap().pts(), first.pts());
        assert_eq!(ring.aux[1].1.packets.front().unwrap().pts(), first.pts());
    }

    #[test]
    fn skips_leading_delta_frames() {
        gst::init().unwrap();
        let mut ring = Ring::new(gst::ClockTime::from_seconds(2));
        ring.push_video(None, buf(0, false));
        assert!(ring.video.packets.is_empty());
        ring.push_video(None, buf(100, true));
        assert_eq!(ring.video.packets.len(), 1);
    }
}
