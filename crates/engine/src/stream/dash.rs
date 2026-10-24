//! DASH manifests (MPD): each representation and the list of its segments,
//! from a `SegmentTemplate` (with or without a timeline), a `SegmentList`,
//! or a single file.

use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use reqwest::Url;

use super::{Container, PieceSpec};
use crate::{Error, Result};

/// More segments than this means a broken manifest, not a long video.
const MAX_SEGMENTS: u64 = 500_000;

#[derive(Debug, Clone)]
pub(crate) struct Mpd {
    /// `type="dynamic"`: a live stream.
    pub dynamic: bool,
    /// Seconds.
    pub duration: Option<f64>,
    pub periods: usize,
    pub reps: Vec<Representation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Video,
    Audio,
}

#[derive(Debug, Clone)]
pub(crate) struct Representation {
    pub id: String,
    pub kind: Kind,
    pub bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub container: Container,
    /// Its adaptation set has the "main" role.
    pub main: bool,
    pub protected: bool,
    pub pieces: Vec<PieceSpec>,
    /// One whole file rather than a list of segments.
    pub single_file: bool,
}

pub(crate) fn parse(text: &str, base: &Url) -> Result<Mpd> {
    let root = parse_xml(text)?;
    let mpd = root
        .children
        .iter()
        .find(|n| n.name == "MPD")
        .ok_or_else(|| Error::Playlist("not a DASH manifest".into()))?;
    let duration = mpd
        .attr("mediaPresentationDuration")
        .and_then(parse_duration);
    if mpd.attr("type") == Some("dynamic") {
        // Live: its segments keep coming, so there is nothing to list yet.
        return Ok(Mpd {
            dynamic: true,
            duration,
            periods: 0,
            reps: Vec::new(),
        });
    }
    let base = base_url(base, mpd)?;
    let periods: Vec<&Node> = mpd.children("Period").collect();
    let period = periods
        .first()
        .ok_or_else(|| Error::Playlist("the manifest has no periods".into()))?;
    let period_duration = period
        .attr("duration")
        .and_then(parse_duration)
        .or_else(|| {
            let start = period.attr("start").and_then(parse_duration).unwrap_or(0.0);
            duration.map(|d| d - start)
        });
    let mut out = Mpd {
        dynamic: false,
        duration: period_duration.or(duration),
        periods: periods.len(),
        reps: Vec::new(),
    };
    let period_base = base_url(&base, period)?;
    for set in period.children("AdaptationSet") {
        let set_base = base_url(&period_base, set)?;
        let main = set
            .children("Role")
            .any(|r| r.attr("value") == Some("main"));
        for rep in set.children("Representation") {
            let mime = rep.attr("mimeType").or(set.attr("mimeType")).unwrap_or("");
            let codecs = rep.attr("codecs").or(set.attr("codecs"));
            let Some(kind) = kind_of(mime, set.attr("contentType"), codecs) else {
                continue;
            };
            let id = rep.attr("id").unwrap_or_default().to_string();
            let bandwidth = rep.attr("bandwidth").and_then(|b| b.parse().ok());
            let rep_base = base_url(&set_base, rep)?;
            let (pieces, single_file) =
                segments([period, set, rep], &rep_base, &id, bandwidth, out.duration)?;
            out.reps.push(Representation {
                kind,
                bandwidth,
                width: rep
                    .attr("width")
                    .or(set.attr("width"))
                    .and_then(|w| w.parse().ok()),
                height: rep
                    .attr("height")
                    .or(set.attr("height"))
                    .and_then(|h| h.parse().ok()),
                codecs: codecs.map(str::to_string),
                container: container_of(mime),
                main,
                protected: set.child("ContentProtection").is_some()
                    || rep.child("ContentProtection").is_some(),
                pieces,
                single_file,
                id,
            });
        }
    }
    if out.reps.is_empty() {
        return Err(Error::Playlist(
            "the manifest lists no video or audio".into(),
        ));
    }
    Ok(out)
}

fn kind_of(mime: &str, content_type: Option<&str>, codecs: Option<&str>) -> Option<Kind> {
    let codecs = codecs.unwrap_or("").to_ascii_lowercase();
    if mime.starts_with("video/") || content_type == Some("video") {
        Some(Kind::Video)
    } else if mime.starts_with("audio/") || content_type == Some("audio") {
        Some(Kind::Audio)
    } else if ["avc", "hvc", "hev", "vp9", "vp09", "av01"]
        .iter()
        .any(|c| codecs.starts_with(c))
    {
        Some(Kind::Video)
    } else if ["mp4a", "opus", "ac-3", "ec-3", "flac", "vorbis"]
        .iter()
        .any(|c| codecs.starts_with(c))
    {
        Some(Kind::Audio)
    } else {
        None
    }
}

fn container_of(mime: &str) -> Container {
    match mime.split('/').nth(1).unwrap_or("") {
        "webm" => Container::Webm,
        "mp2t" => Container::Ts,
        _ => Container::Mp4,
    }
}

/// The representation's pieces, and whether it is one whole file.
fn segments(
    levels: [&Node; 3],
    base: &Url,
    id: &str,
    bandwidth: Option<u64>,
    duration: Option<f64>,
) -> Result<(Vec<PieceSpec>, bool)> {
    let piece = |url: Url, range: Option<(u64, u64)>, seconds: f64| PieceSpec {
        url,
        range,
        key: None,
        seconds,
    };
    let join = |uri: &str| {
        base.join(uri.trim())
            .map_err(|e| Error::Playlist(format!("a bad link ({uri}): {e}")))
    };
    // The innermost level wins, attribute by attribute.
    let innermost = |name: &str| levels.iter().rev().find_map(|l| l.child(name));

    let templates: Vec<&Node> = levels
        .iter()
        .filter_map(|l| l.child("SegmentTemplate"))
        .collect();
    if !templates.is_empty() {
        let attr = |name: &str| templates.iter().rev().find_map(|t| t.attr(name));
        let number = |name: &str, default: u64| {
            attr(name)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(default)
        };
        let media = attr("media")
            .ok_or_else(|| Error::Playlist("a segment template without media".into()))?;
        let start_number = number("startNumber", 1);
        let timescale = number("timescale", 1).max(1);
        let offset = number("presentationTimeOffset", 0);
        let mut pieces = Vec::new();
        if let Some(init) = attr("initialization") {
            pieces.push(piece(join(&expand(init, id, bandwidth, 0, 0))?, None, 0.0));
        }
        let timeline = templates
            .iter()
            .rev()
            .find_map(|t| t.child("SegmentTimeline"));
        let times = match timeline {
            Some(timeline) => {
                let end = duration.map(|d| offset + (d * timescale as f64).round() as u64);
                timeline_times(timeline, start_number, end)?
            }
            None => {
                let step = number("duration", 0);
                let total = duration.ok_or_else(|| {
                    Error::Playlist("the manifest does not say how long it is".into())
                })?;
                if step == 0 {
                    return Err(Error::Playlist(
                        "a segment template without a duration".into(),
                    ));
                }
                let count = ((total * timescale as f64) / step as f64 - 1e-6)
                    .ceil()
                    .max(1.0) as u64;
                if count > MAX_SEGMENTS {
                    return Err(Error::Playlist("too many segments".into()));
                }
                (0..count)
                    .map(|i| (start_number + i, offset + i * step, step))
                    .collect()
            }
        };
        for (n, t, ticks) in times {
            let seconds = ticks as f64 / timescale as f64;
            pieces.push(piece(
                join(&expand(media, id, bandwidth, n, t))?,
                None,
                seconds,
            ));
        }
        return Ok((pieces, false));
    }

    if let Some(list) = innermost("SegmentList") {
        let mut pieces = Vec::new();
        if let Some(init) = list.child("Initialization") {
            let url = init.attr("sourceURL").map_or(Ok(base.clone()), join)?;
            pieces.push(piece(url, init.attr("range").and_then(parse_range), 0.0));
        }
        let ticks = |name: &str| list.attr(name).and_then(|v| v.parse::<f64>().ok());
        let seconds =
            ticks("duration").map_or(0.0, |d| d / ticks("timescale").unwrap_or(1.0).max(1.0));
        for seg in list.children("SegmentURL") {
            let url = seg.attr("media").map_or(Ok(base.clone()), join)?;
            pieces.push(piece(
                url,
                seg.attr("mediaRange").and_then(parse_range),
                seconds,
            ));
        }
        if pieces.is_empty() {
            return Err(Error::Playlist("an empty segment list".into()));
        }
        return Ok((pieces, false));
    }

    // SegmentBase, or nothing: the representation is one complete file.
    Ok((
        vec![piece(base.clone(), None, duration.unwrap_or(0.0))],
        true,
    ))
}

/// (number, time, duration) of each segment in a `SegmentTimeline`.
fn timeline_times(
    timeline: &Node,
    start_number: u64,
    end: Option<u64>,
) -> Result<Vec<(u64, u64, u64)>> {
    let entries: Vec<&Node> = timeline.children("S").collect();
    let mut out = Vec::new();
    let (mut number, mut t) = (start_number, 0u64);
    for (i, s) in entries.iter().enumerate() {
        let parse = |name: &str| s.attr(name).and_then(|v| v.parse::<i64>().ok());
        if let Some(start) = parse("t") {
            t = start.max(0) as u64;
        }
        let d = parse("d")
            .filter(|d| *d > 0)
            .ok_or_else(|| Error::Playlist("a timeline entry without a duration".into()))?
            as u64;
        let repeat = match parse("r").unwrap_or(0) {
            // Repeat until the next entry starts, or the period ends.
            r if r < 0 => {
                let until = entries
                    .get(i + 1)
                    .and_then(|n| n.attr("t"))
                    .and_then(|v| v.parse::<u64>().ok())
                    .or(end);
                match until {
                    Some(until) if until > t => (until - t).div_ceil(d) - 1,
                    _ => 0,
                }
            }
            r => r as u64,
        };
        if out.len() as u64 + repeat > MAX_SEGMENTS {
            return Err(Error::Playlist("too many segments".into()));
        }
        for _ in 0..=repeat {
            out.push((number, t, d));
            number += 1;
            t += d;
        }
    }
    Ok(out)
}

/// Fills in `$RepresentationID$`, `$Number$`, `$Number%05d$`,
/// `$Bandwidth$`, `$Time$` and `$$`.
fn expand(template: &str, id: &str, bandwidth: Option<u64>, number: u64, time: u64) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let tag = &after[..end];
        rest = &after[end + 1..];
        if tag.is_empty() {
            out.push('$');
            continue;
        }
        let (name, width) = match tag.split_once('%') {
            Some((name, format)) => (
                name,
                format
                    .trim_end_matches('d')
                    .trim_start_matches('0')
                    .parse::<usize>()
                    .ok(),
            ),
            None => (tag, None),
        };
        let value = match name {
            "RepresentationID" => id.to_string(),
            "Number" => number.to_string(),
            "Bandwidth" => bandwidth.unwrap_or(0).to_string(),
            "Time" => time.to_string(),
            _ => {
                out.push('$');
                out.push_str(tag);
                out.push('$');
                continue;
            }
        };
        match width {
            Some(width) => out.push_str(&format!("{value:0>width$}")),
            None => out.push_str(&value),
        }
    }
    out.push_str(rest);
    out
}

/// `first-last` → (offset, length).
fn parse_range(range: &str) -> Option<(u64, u64)> {
    let (a, b) = range.trim().split_once('-')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    (b >= a).then(|| (a, b - a + 1))
}

/// ISO 8601 durations such as `PT1H2M3.5S` or `P1DT2H`, in seconds.
fn parse_duration(text: &str) -> Option<f64> {
    let text = text.trim().strip_prefix('P')?;
    let (date, time) = text.split_once('T').unwrap_or((text, ""));
    let mut seconds = 0.0;
    let mut add = |part: &str, units: &[(char, f64)]| -> Option<()> {
        let mut number = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                number.push(c);
            } else {
                let unit = units.iter().find(|(u, _)| *u == c)?.1;
                seconds += number.parse::<f64>().ok()? * unit;
                number.clear();
            }
        }
        number.is_empty().then_some(())
    };
    add(
        date,
        &[
            ('Y', 365.0 * 86400.0),
            ('M', 30.0 * 86400.0),
            ('W', 7.0 * 86400.0),
            ('D', 86400.0),
        ],
    )?;
    add(time, &[('H', 3600.0), ('M', 60.0), ('S', 1.0)])?;
    Some(seconds)
}

fn base_url(parent: &Url, node: &Node) -> Result<Url> {
    match node.child("BaseURL").map(|b| b.text.trim()) {
        Some(text) if !text.is_empty() => parent
            .join(text)
            .map_err(|e| Error::Playlist(format!("a bad base URL ({text}): {e}"))),
        _ => Ok(parent.clone()),
    }
}

/// Just enough of an XML tree for reading manifests.
#[derive(Debug, Default)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
    text: String,
}

impl Node {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |c| c.name == name)
    }
}

fn parse_xml(text: &str) -> Result<Node> {
    let bad = |e: &dyn std::fmt::Display| Error::Playlist(format!("not a valid manifest: {e}"));
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack = vec![Node::default()];
    loop {
        match reader.read_event().map_err(|e| bad(&e))? {
            Event::Start(e) => stack.push(element(&e).map_err(|e| bad(&e))?),
            Event::Empty(e) => {
                let node = element(&e).map_err(|e| bad(&e))?;
                stack.last_mut().expect("root").children.push(node);
            }
            Event::End(_) => {
                let mut node = stack.pop().expect("root");
                let Some(parent) = stack.last_mut() else {
                    return Err(bad(&"unbalanced tags"));
                };
                if let Ok(text) = quick_xml::escape::unescape(&node.text) {
                    node.text = text.into_owned();
                }
                parent.children.push(node);
            }
            Event::Text(t) => {
                let text = t.xml_content(XmlVersion::Implicit1_0);
                stack.last_mut().expect("root").text.push_str(&text);
            }
            Event::GeneralRef(r) => {
                let name = r.xml_content(XmlVersion::Implicit1_0);
                stack
                    .last_mut()
                    .expect("root")
                    .text
                    .push_str(&format!("&{name};"));
            }
            Event::CData(c) => {
                let raw = c.xml_content(XmlVersion::Implicit1_0);
                let text = quick_xml::escape::escape(raw.as_ref()).into_owned();
                stack.last_mut().expect("root").text.push_str(&text);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    match stack.pop() {
        Some(root) if stack.is_empty() => Ok(root),
        _ => Err(bad(&"unclosed tags")),
    }
}

fn element(e: &BytesStart) -> std::result::Result<Node, quick_xml::Error> {
    let mut node = Node {
        name: e.local_name().as_ref().to_string(),
        ..Node::default()
    };
    for attr in e.attributes() {
        let attr = attr.map_err(quick_xml::Error::from)?;
        let key = attr.key.local_name().as_ref().to_string();
        let value = attr.normalized_value(XmlVersion::Implicit1_0)?.into_owned();
        node.attrs.push((key, value));
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/movie/manifest.mpd").unwrap()
    }

    fn names(pieces: &[PieceSpec]) -> Vec<String> {
        pieces
            .iter()
            .map(|p| p.url.path().rsplit('/').next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn reads_durations() {
        assert_eq!(parse_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(parse_duration("PT634.566S"), Some(634.566));
        assert_eq!(parse_duration("P0Y0M0DT0H3M30.000S"), Some(210.0));
        assert_eq!(parse_duration("P1DT1S"), Some(86401.0));
        assert_eq!(parse_duration("1H"), None);
    }

    #[test]
    fn fills_in_templates() {
        assert_eq!(
            expand("$RepresentationID$/seg-$Number%05d$.m4s", "v1", None, 42, 0),
            "v1/seg-00042.m4s"
        );
        assert_eq!(
            expand("t$Time$-$Bandwidth$$$.mp4", "a", Some(128000), 1, 9000),
            "t9000-128000$.mp4"
        );
        assert_eq!(expand("odd$Unknown$", "a", None, 1, 0), "odd$Unknown$");
    }

    #[test]
    fn counts_segments_from_the_duration() {
        let text = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10.5S">
  <BaseURL>media/</BaseURL>
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <SegmentTemplate initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/$Number$.m4s" startNumber="1" timescale="1000" duration="4000"/>
      <Representation id="720p" bandwidth="2000000" width="1280" height="720" codecs="avc1.64001f"/>
      <Representation id="360p" bandwidth="600000" width="640" height="360"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <Representation id="aac" bandwidth="128000" codecs="mp4a.40.2">
        <SegmentTemplate initialization="a-init.mp4" media="a-$Number%03d$.m4s" timescale="48000" duration="192000"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="text/vtt"><Representation id="subs"/></AdaptationSet>
  </Period>
</MPD>"#;
        let mpd = parse(text, &base()).unwrap();
        assert!(!mpd.dynamic);
        assert_eq!(mpd.duration, Some(10.5));
        assert_eq!(mpd.reps.len(), 3, "subtitles are left out");
        let hd = &mpd.reps[0];
        assert_eq!(
            (hd.kind, hd.height, hd.bandwidth),
            (Kind::Video, Some(720), Some(2_000_000))
        );
        assert_eq!(
            hd.pieces[0].url.as_str(),
            "https://cdn.example/movie/media/720p/init.mp4"
        );
        assert_eq!(names(&hd.pieces), ["init.mp4", "1.m4s", "2.m4s", "3.m4s"]);
        let audio = &mpd.reps[2];
        assert!(audio.main && audio.kind == Kind::Audio);
        assert_eq!(
            names(&audio.pieces),
            ["a-init.mp4", "a-001.m4s", "a-002.m4s", "a-003.m4s"]
        );
    }

    #[test]
    fn follows_a_timeline() {
        let text = r#"<MPD type="static" mediaPresentationDuration="PT20S"><Period>
  <AdaptationSet mimeType="video/mp4">
    <SegmentTemplate timescale="10" media="v_$Time$.mp4" initialization="v_init.mp4" startNumber="5">
      <SegmentTimeline><S t="100" d="40" r="2"/><S d="30"/><S d="20" r="-1"/></SegmentTimeline>
    </SegmentTemplate>
    <Representation id="v" bandwidth="1000"/>
  </AdaptationSet></Period></MPD>"#;
        let mpd = parse(text, &base()).unwrap();
        // 100, 140, 180 (r=2), 220 (d=30), then an open repeat starting
        // past the period's end (200 ticks), which still gives one segment.
        let names = names(&mpd.reps[0].pieces);
        assert_eq!(
            names[..5],
            [
                "v_init.mp4",
                "v_100.mp4",
                "v_140.mp4",
                "v_180.mp4",
                "v_220.mp4"
            ]
        );
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn reads_segment_lists_and_single_files() {
        let text = r#"<MPD mediaPresentationDuration="PT8S"><Period>
  <AdaptationSet mimeType="video/webm">
    <Representation id="list" bandwidth="1">
      <BaseURL>https://media.example/one.webm?sig=a&amp;b=c</BaseURL>
      <SegmentList><Initialization range="0-99"/><SegmentURL mediaRange="100-499"/><SegmentURL mediaRange="500-899"/></SegmentList>
    </Representation>
    <Representation id="whole" bandwidth="2"><BaseURL>whole.webm</BaseURL><SegmentBase indexRange="0-10"/></Representation>
  </AdaptationSet></Period></MPD>"#;
        let mpd = parse(text, &base()).unwrap();
        let list = &mpd.reps[0];
        assert_eq!(list.container, Container::Webm);
        assert_eq!(
            list.pieces[0].url.as_str(),
            "https://media.example/one.webm?sig=a&b=c"
        );
        let ranges: Vec<_> = list.pieces.iter().map(|p| p.range).collect();
        assert_eq!(ranges, [Some((0, 100)), Some((100, 400)), Some((500, 400))]);
        assert!(!list.single_file);
        let whole = &mpd.reps[1];
        assert!(whole.single_file);
        assert_eq!(
            whole.pieces[0].url.as_str(),
            "https://cdn.example/movie/whole.webm"
        );
    }

    #[test]
    fn notices_live_and_protected_streams() {
        let text = r#"<MPD type="dynamic"><Period><AdaptationSet mimeType="video/mp4">
  <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>
  <SegmentTemplate media="$Number$.m4s" duration="2" timescale="1"/>
  <Representation id="v"/></AdaptationSet></Period></MPD>"#;
        assert!(parse(text, &base()).unwrap().dynamic);
        let protected = text.replace(r#"type="dynamic""#, r#"mediaPresentationDuration="PT4S""#);
        assert!(parse(&protected, &base()).unwrap().reps[0].protected);
        assert!(parse("<html><body>hi</body></html>", &base()).is_err());
    }
}
