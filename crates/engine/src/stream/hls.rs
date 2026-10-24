//! HLS playlists (RFC 8216): the qualities a master playlist offers, and the
//! segments of a media playlist.

use reqwest::Url;

use super::{Container, KeySpec, PieceSpec};
use crate::{Error, Result};

#[derive(Debug, Clone)]
pub(crate) enum Playlist {
    Master(Master),
    Media(Media),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Master {
    pub variants: Vec<HlsVariant>,
    /// A session key that only DRM can open.
    pub protected: bool,
}

/// One quality, as listed by `#EXT-X-STREAM-INF`.
#[derive(Debug, Clone)]
pub(crate) struct HlsVariant {
    /// The URI exactly as written, which stays the same when the master
    /// playlist's own link changes (signed links); used to find it again.
    pub uri: String,
    pub url: Url,
    pub bandwidth: Option<u64>,
    pub average_bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    /// A separate audio playlist to join with this one.
    pub audio: Option<Rendition>,
}

#[derive(Debug, Clone)]
pub(crate) struct Rendition {
    pub uri: String,
    pub url: Url,
}

#[derive(Debug, Clone)]
pub(crate) struct Media {
    pub pieces: Vec<PieceSpec>,
    /// Seconds, the sum of the segments' durations.
    pub duration: f64,
    /// `#EXT-X-ENDLIST`: the stream is complete (not live).
    pub ended: bool,
    pub protected: bool,
    pub container: Container,
}

pub(crate) fn parse(text: &str, base: &Url) -> Result<Playlist> {
    let text = text.trim_start_matches('\u{feff}');
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err(Error::Playlist("not an HLS playlist".into()));
    }
    if text.contains("#EXT-X-STREAM-INF") {
        parse_master(text, base).map(Playlist::Master)
    } else {
        parse_media(text, base).map(Playlist::Media)
    }
}

fn parse_master(text: &str, base: &Url) -> Result<Master> {
    struct AudioMedia {
        group: String,
        default: bool,
        uri: Option<String>,
    }
    let mut master = Master::default();
    let mut audio = Vec::new();
    let mut pending: Option<Vec<(String, String)>> = None;
    let mut streams = Vec::new();
    for line in lines(text) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attributes(attrs));
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = attributes(attrs);
            if get(&attrs, "TYPE") == Some("AUDIO") {
                audio.push(AudioMedia {
                    group: get(&attrs, "GROUP-ID").unwrap_or_default().to_string(),
                    default: get(&attrs, "DEFAULT") == Some("YES"),
                    uri: get(&attrs, "URI").map(str::to_string),
                });
            }
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-SESSION-KEY:") {
            master.protected |= is_drm(&attributes(attrs));
        } else if !line.starts_with('#')
            && let Some(attrs) = pending.take()
        {
            streams.push((line.to_string(), attrs));
        }
    }
    for (uri, attrs) in streams {
        if master.variants.iter().any(|v| v.uri == uri) {
            continue;
        }
        let (width, height) = get(&attrs, "RESOLUTION")
            .and_then(|r| r.split_once(['x', 'X']))
            .map_or((None, None), |(w, h)| (w.parse().ok(), h.parse().ok()));
        // The sound a player picks: the group's default, else its first.
        // A rendition without a URI is inside the variant's own segments.
        let rendition = get(&attrs, "AUDIO").and_then(|group| {
            let in_group = || audio.iter().filter(move |a| a.group == group);
            in_group()
                .find(|a| a.default)
                .or_else(|| in_group().next())
                .and_then(|a| a.uri.clone())
        });
        master.variants.push(HlsVariant {
            url: join(base, &uri)?,
            uri,
            bandwidth: get(&attrs, "BANDWIDTH").and_then(|b| b.parse().ok()),
            average_bandwidth: get(&attrs, "AVERAGE-BANDWIDTH").and_then(|b| b.parse().ok()),
            width,
            height,
            codecs: get(&attrs, "CODECS").map(str::to_string),
            audio: rendition
                .map(|uri| {
                    Ok::<_, Error>(Rendition {
                        url: join(base, &uri)?,
                        uri,
                    })
                })
                .transpose()?,
        });
    }
    if master.variants.is_empty() {
        return Err(Error::Playlist("the playlist lists no streams".into()));
    }
    Ok(master)
}

fn parse_media(text: &str, base: &Url) -> Result<Media> {
    let mut media = Media {
        pieces: Vec::new(),
        duration: 0.0,
        ended: false,
        protected: false,
        container: Container::Ts,
    };
    let mut sequence: u64 = 0;
    let mut key: Option<(Url, Option<[u8; 16]>)> = None;
    let mut map: Option<PieceSpec> = None;
    let mut last_map: Option<PieceSpec> = None;
    let mut range: Option<(u64, Option<u64>)> = None;
    // Where the previous byte range ended, for ranges without an offset.
    let mut range_end: Option<(Url, u64)> = None;
    let mut saw_segment = false;
    let mut first_uri = None;
    let mut seconds = 0.0;
    for line in lines(text) {
        if let Some(n) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = n.trim().parse().unwrap_or(0);
        } else if let Some(d) = line.strip_prefix("#EXTINF:") {
            let d = d.split(',').next().unwrap_or("").trim();
            seconds = d.parse::<f64>().unwrap_or(0.0);
            media.duration += seconds;
        } else if line == "#EXT-X-ENDLIST" {
            media.ended = true;
        } else if let Some(kind) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            media.ended |= kind.trim() == "VOD";
        } else if let Some(r) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = parse_byte_range(r);
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = attributes(attrs);
            if is_drm(&attrs) {
                media.protected = true;
            }
            key = match get(&attrs, "METHOD") {
                Some("AES-128") if !media.protected => {
                    let uri = get(&attrs, "URI")
                        .ok_or_else(|| Error::Playlist("a key without a URI".into()))?;
                    let iv = get(&attrs, "IV").map(parse_iv).transpose()?;
                    Some((join(base, uri)?, iv))
                }
                _ => None,
            };
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = attributes(attrs);
            let uri = get(&attrs, "URI")
                .ok_or_else(|| Error::Playlist("an init section without a URI".into()))?;
            let url = join(base, uri)?;
            let range = get(&attrs, "BYTERANGE")
                .and_then(parse_byte_range)
                .map(|(len, offset)| (offset.unwrap_or(0), len));
            // An encrypted init section must name its IV.
            let key = key.as_ref().and_then(|(url, iv)| {
                iv.map(|iv| KeySpec {
                    url: url.clone(),
                    iv,
                })
            });
            map = Some(PieceSpec {
                url,
                range,
                key,
                seconds: 0.0,
            });
            media.container = Container::Mp4;
        } else if !line.starts_with('#') {
            let url = join(base, line)?;
            first_uri.get_or_insert_with(|| url.path().to_ascii_lowercase());
            if let Some(init) = &map
                && last_map.as_ref() != Some(init)
            {
                media.pieces.push(init.clone());
                last_map = Some(init.clone());
            }
            let piece_range = range.take().map(|(len, offset)| {
                let offset = offset.unwrap_or(match &range_end {
                    Some((u, end)) if *u == url => *end,
                    _ => 0,
                });
                range_end = Some((url.clone(), offset + len));
                (offset, len)
            });
            media.pieces.push(PieceSpec {
                key: key.as_ref().map(|(key_url, iv)| KeySpec {
                    url: key_url.clone(),
                    iv: iv.unwrap_or_else(|| (sequence as u128).to_be_bytes()),
                }),
                url,
                range: piece_range,
                seconds: std::mem::take(&mut seconds),
            });
            sequence += 1;
            saw_segment = true;
        }
    }
    if !saw_segment {
        return Err(Error::Playlist("the playlist has no segments".into()));
    }
    if map.is_none()
        && let Some(path) = first_uri
    {
        media.container = Container::from_extension(path.rsplit('.').next().unwrap_or(""))
            .unwrap_or(Container::Ts);
    }
    Ok(media)
}

/// Encryption that needs a DRM licence rather than a plain key file.
fn is_drm(attrs: &[(String, String)]) -> bool {
    let method = get(attrs, "METHOD").unwrap_or("NONE");
    let format = get(attrs, "KEYFORMAT").unwrap_or("identity");
    method != "NONE" && (method != "AES-128" || format != "identity")
}

/// `<length>[@<offset>]`
fn parse_byte_range(r: &str) -> Option<(u64, Option<u64>)> {
    let r = r.trim().trim_matches('"');
    match r.split_once('@') {
        Some((len, offset)) => Some((len.parse().ok()?, Some(offset.parse().ok()?))),
        None => Some((r.parse().ok()?, None)),
    }
}

fn parse_iv(iv: &str) -> Result<[u8; 16]> {
    let hex = iv
        .strip_prefix("0x")
        .or_else(|| iv.strip_prefix("0X"))
        .unwrap_or(iv);
    u128::from_str_radix(hex, 16)
        .map(u128::to_be_bytes)
        .map_err(|_| Error::Playlist(format!("a bad IV: {iv}")))
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty())
}

fn join(base: &Url, uri: &str) -> Result<Url> {
    base.join(uri.trim())
        .map_err(|e| Error::Playlist(format!("a bad link ({uri}): {e}")))
}

/// Parses `KEY=value,KEY="quoted, value"` lists.
fn attributes(list: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let (value, after) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((value, after)) => (value, after),
                None => (quoted, ""),
            }
        } else {
            after.split_once(',').map_or((after, ""), |(v, a)| (v, a))
        };
        out.push((key.trim().to_ascii_uppercase(), value.trim().to_string()));
        rest = after.trim_start_matches([',', ' ']);
    }
    out
}

fn get<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/video/master.m3u8?token=abc").unwrap()
    }

    #[test]
    fn reads_quoted_attributes_with_commas() {
        let attrs =
            attributes(r#"BANDWIDTH=1280000,CODECS="avc1.4d401f,mp4a.40.2",RESOLUTION=1280x720"#);
        assert_eq!(get(&attrs, "CODECS"), Some("avc1.4d401f,mp4a.40.2"));
        assert_eq!(get(&attrs, "RESOLUTION"), Some("1280x720"));
    }

    #[test]
    fn lists_qualities_with_their_audio() {
        let text = "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,URI=\"audio/en.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"French\",URI=\"audio/fr.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e\",AUDIO=\"aud\"
360p/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,AVERAGE-BANDWIDTH=2000000,RESOLUTION=1280x720,AUDIO=\"aud\"
https://other.example/720p.m3u8
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=100000,URI=\"iframes.m3u8\"
";
        let Playlist::Master(m) = parse(text, &base()).unwrap() else {
            panic!("expected a master playlist");
        };
        assert_eq!(m.variants.len(), 2);
        let v = &m.variants[0];
        assert_eq!(v.url.as_str(), "https://cdn.example/video/360p/index.m3u8");
        assert_eq!(
            (v.width, v.height, v.bandwidth),
            (Some(640), Some(360), Some(800_000))
        );
        assert_eq!(
            v.audio.as_ref().unwrap().url.as_str(),
            "https://cdn.example/video/audio/en.m3u8"
        );
        assert_eq!(m.variants[1].average_bandwidth, Some(2_000_000));
        assert!(!m.protected);
    }

    #[test]
    fn default_sound_inside_the_video_is_not_separate() {
        let text = "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Main\",DEFAULT=YES
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Commentary\",URI=\"alt/index.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=800000,AUDIO=\"a\"
v.m3u8
";
        let Playlist::Master(m) = parse(text, &base()).unwrap() else {
            panic!("expected a master playlist");
        };
        assert!(m.variants[0].audio.is_none());
    }

    #[test]
    fn reads_segments_keys_and_ranges() {
        let text = "#EXTM3U
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"
#EXTINF:6.0,
a.ts
#EXT-X-KEY:METHOD=AES-128,URI=\"key2.bin\",IV=0x000102030405060708090a0b0c0d0e0f
#EXTINF:4.5,
#EXT-X-BYTERANGE:1000@0
all.ts
#EXTINF:4.5,
#EXT-X-BYTERANGE:500
all.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:1,
plain.ts
#EXT-X-ENDLIST
";
        let Playlist::Media(m) = parse(text, &base()).unwrap() else {
            panic!("expected a media playlist");
        };
        assert!(m.ended && !m.protected);
        assert_eq!(m.container, Container::Ts);
        assert!((m.duration - 16.0).abs() < 1e-9);
        assert_eq!(m.pieces.len(), 4);
        let key = m.pieces[0].key.as_ref().unwrap();
        assert_eq!(key.url.as_str(), "https://cdn.example/video/key.bin");
        // Without an IV, the segment's sequence number is the IV.
        assert_eq!(key.iv, 7u128.to_be_bytes());
        assert_eq!(m.pieces[1].key.as_ref().unwrap().iv[15], 0x0f);
        assert_eq!(m.pieces[1].range, Some((0, 1000)));
        assert_eq!(m.pieces[2].range, Some((1000, 500)));
        assert!(m.pieces[3].key.is_none());
    }

    #[test]
    fn puts_the_init_section_first() {
        let text = "#EXTM3U
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:2,
s1.m4s
#EXTINF:2,
s2.m4s
";
        let Playlist::Media(m) = parse(text, &base()).unwrap() else {
            panic!("expected a media playlist");
        };
        assert!(m.ended);
        assert_eq!(m.container, Container::Mp4);
        let names: Vec<_> = m
            .pieces
            .iter()
            .map(|p| p.url.path().rsplit('/').next().unwrap())
            .collect();
        assert_eq!(names, ["init.mp4", "s1.m4s", "s2.m4s"]);
    }

    #[test]
    fn notices_live_streams_and_drm() {
        let live = "#EXTM3U\n#EXTINF:2,\na.ts\n#EXTINF:2,\nb.ts\n";
        let Playlist::Media(m) = parse(live, &base()).unwrap() else {
            panic!()
        };
        assert!(!m.ended);

        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:2,\na.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(drm, &base()).unwrap() else {
            panic!()
        };
        assert!(m.protected);
        assert!(m.pieces[0].key.is_none());

        assert!(parse("<html>", &base()).is_err());
    }
}
