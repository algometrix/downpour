//! Choosing a local file name from what the browser, URL, and server say.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use url::Url;

const FALLBACK_NAME: &str = "download";
/// For a server that sends a blank name when no URL has a name to use either.
const BLANK_SERVER_NAME: &str = "unnamed";
const NON_BREAKING_SPACE: char = '\u{a0}';
const MAX_NAME_CHARS: usize = 200;
/// Longer than any real extension; a longer "extension" is just text after a dot.
const MAX_EXTENSION_CHARS: usize = 16;
/// A full-length name escaped twice still fits. Each decoding pass is linear and
/// nested escapes need many passes, so this bounds the work a hostile name can
/// cause.
const MAX_ESCAPED_NAME_CHARS: usize = 4096;

/// Where a file name came from, strongest first. Only the last two can be
/// replaced once the server has answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileNameSource {
    User,
    Browser,
    Server,
    /// Worked out before the download's own response: from the URL, or from
    /// what a link check found.
    #[serde(alias = "url")]
    Guess,
    /// The URL has no name, so the value only stands in for a real one.
    Placeholder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileName {
    pub value: String,
    pub source: FileNameSource,
}

impl FileName {
    /// Names from the web lose their URL and HTML escapes. A name the user
    /// typed is kept literally: `50%20off.mp4` may be exactly what they want.
    pub fn new(raw: &str, source: FileNameSource) -> Self {
        let value = match source {
            FileNameSource::User => sanitize_file_name(raw),
            FileNameSource::Browser
            | FileNameSource::Server
            | FileNameSource::Guess
            | FileNameSource::Placeholder => sanitize_file_name(&decode_web_escapes(raw)),
        };
        Self { value, source }
    }

    /// This name with web escapes cleaned, if it still has any. A list saved
    /// before names were cleaned can hold `My%20Clip.mp4`; a typed name stays.
    pub(crate) fn cleaned_of_web_escapes(&self) -> Option<Self> {
        if self.source == FileNameSource::User {
            return None;
        }
        let cleaned = Self::from_web(&self.value, self.source)?;
        (cleaned.value != self.value).then_some(cleaned)
    }

    /// The name someone typed, kept literally, unless it cannot name a file:
    /// `...` is no choice at all, so it must not block the server's name.
    pub fn from_typed(raw: &str) -> Option<Self> {
        usable_file_name(raw).map(|value| Self {
            value,
            source: FileNameSource::User,
        })
    }

    /// The name someone typed over this offered one. A browser or server
    /// knows the file's kind, so like a rename its extension stays: leaving
    /// it in the box (`clip.mp4` or `clip.MP4`) keeps it once, and any other
    /// dot is part of the stem (`v1.2` over `a.mp4` is `v1.2.mp4`). A guess
    /// from the address (`download.php`) may be wrong, so a typed extension
    /// replaces it and only a bare stem takes it. `None` when the typed text
    /// cannot name a file.
    pub fn typed_over(&self, typed: &str) -> Option<FileName> {
        let typed = typed.trim();
        let Some(offered_extension) = NameParts::of(&self.value).extension else {
            return Self::from_typed(typed);
        };
        let typed_parts = NameParts::of(typed);
        let stem = match typed_parts.extension {
            Some(extension) if extension.eq_ignore_ascii_case(offered_extension) => {
                typed_parts.stem
            }
            Some(_) if !self.knows_extension() => return Self::from_typed(typed),
            _ => typed,
        };
        ChosenStem::from_typed(stem).map(|stem| stem.named_like(&self.value))
    }

    /// Whether this name's extension says what the file is, rather than
    /// being read off the address.
    fn knows_extension(&self) -> bool {
        match self.source {
            FileNameSource::User | FileNameSource::Browser | FileNameSource::Server => true,
            FileNameSource::Guess | FileNameSource::Placeholder => false,
        }
    }

    /// The name a browser or page offers, unless nothing usable survives
    /// cleaning: `%20` names no file, so the caller should guess instead.
    pub fn from_browser(raw: &str) -> Option<Self> {
        Self::from_web(raw, FileNameSource::Browser)
    }

    /// The name a server gave, unless it is blank: `filename=""` or `"%20"`.
    pub fn from_server(raw: &str) -> Option<Self> {
        Self::from_web(raw, FileNameSource::Server)
    }

    /// The name in a URL's path, unless the path has none: `https://host/`.
    pub fn from_url_path(url: &Url) -> Option<Self> {
        Self::from_web(last_path_segment(url), FileNameSource::Guess)
    }

    fn from_web(raw: &str, source: FileNameSource) -> Option<Self> {
        usable_file_name(&decode_web_escapes(raw)).map(|value| Self { value, source })
    }

    fn placeholder() -> Self {
        Self {
            value: FALLBACK_NAME.to_owned(),
            source: FileNameSource::Placeholder,
        }
    }

    /// The name to save under once a download's first response has arrived.
    /// `header` is the name in its `Content-Disposition`, if it had one.
    ///
    /// A name someone chose is kept. Otherwise a real server name wins, then
    /// the guess, then the name in the URL redirects ended at. `unnamed` is
    /// only for a server that sent a blank name with nothing to fall back on.
    pub fn after_response(&self, header: Option<&str>, final_url: &Url) -> Self {
        let server_name = header.and_then(Self::from_server);
        match self.source {
            FileNameSource::User | FileNameSource::Browser | FileNameSource::Server => self.clone(),
            FileNameSource::Guess => server_name.unwrap_or_else(|| self.clone()),
            FileNameSource::Placeholder => server_name
                .or_else(|| Self::from_url_path(final_url))
                .unwrap_or_else(|| match header {
                    Some(_) => Self {
                        value: BLANK_SERVER_NAME.to_owned(),
                        source: FileNameSource::Server,
                    },
                    None => self.clone(),
                }),
        }
    }

    /// The name to show once `target` is reserved: taking a free name may have
    /// numbered it, as in `clip (1).mp4`.
    pub fn saved_as(&self, target: &Path) -> Self {
        match target.file_name() {
            Some(name) => Self {
                value: name.to_string_lossy().into_owned(),
                source: self.source,
            },
            None => self.clone(),
        }
    }

    pub fn guess_from_url(url: &Url) -> Self {
        Self::from_url_path(url).unwrap_or_else(Self::placeholder)
    }

    /// Manifests are usually named `master.m3u8` or `manifest.mpd`; the
    /// video's name, when the URL has one, is a folder further up the path.
    pub fn guess_for_stream(url: &Url) -> Self {
        let meaningful = url
            .path_segments()
            .into_iter()
            .flatten()
            .rev()
            .map(decode_web_escapes)
            .map(|segment| match segment.rsplit_once('.') {
                Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
                _ => segment,
            })
            .find(|stem| !is_generic_stream_name(stem));
        Self::new(
            meaningful.as_deref().unwrap_or(STREAM_FALLBACK_NAME),
            FileNameSource::Guess,
        )
    }
}

fn last_path_segment(url: &Url) -> &str {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default()
}

const STREAM_FALLBACK_NAME: &str = "video";
/// Path segments that name a playlist's role rather than the video.
const GENERIC_STREAM_NAMES: [&str; 12] = [
    "master",
    "index",
    "playlist",
    "manifest",
    "chunklist",
    "main",
    "stream",
    "video",
    "prog_index",
    "hls",
    "dash",
    "media",
];

fn is_generic_stream_name(stem: &str) -> bool {
    let lower = stem.to_ascii_lowercase();
    let is_quality = lower
        .strip_suffix('p')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()));
    lower.is_empty()
        || GENERIC_STREAM_NAMES.contains(&lower.as_str())
        || lower.chars().all(|c| c.is_ascii_digit())
        || is_quality
}

/// Undoes URL and HTML escaping: `Tom%20%26amp%3B%20Jerry` becomes `Tom & Jerry`.
/// The result is untrusted (`%2F` becomes `/`), so sanitize it afterwards.
///
/// Decoding runs until nothing changes, however many times the name was
/// escaped. `FileName::new` runs again on names that are already clean, and
/// this is what keeps a name from changing after the user has seen it.
fn decode_web_escapes(raw: &str) -> String {
    let mut name = last_chars(raw, MAX_ESCAPED_NAME_CHARS).to_owned();
    loop {
        let decoded = decode_escape_layer(&name);
        if decoded == name {
            return name.replace(NON_BREAKING_SPACE, " ");
        }
        name = decoded;
    }
}

/// The end of a name holds what matters most: its base name and extension.
fn last_chars(text: &str, count: usize) -> &str {
    text.char_indices()
        .rev()
        .nth(count - 1)
        .map_or(text, |(start, _)| &text[start..])
}

fn decode_escape_layer(name: &str) -> String {
    let bytes: Vec<u8> = percent_decode_str(name).collect();
    let mut decoded = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        decoded.push_str(chunk.valid());
        // Escapes that are not UTF-8 stay escaped, because `caf%E9` is more
        // useful in a file name than a replacement character.
        for byte in chunk.invalid() {
            decoded.push_str(&format!("%{byte:02X}"));
        }
    }
    // Removed here as well as when sanitizing: taking one out of `%<control>20`
    // creates a new escape, which has to be decoded before the name is final.
    html_escape::decode_html_entities(&decoded).replace(is_bidi_control, "")
}

/// These reorder how text is displayed, so `photo<U+202E>gpj.exe` shows as
/// `photoexe.jpg` while Windows runs it as a program. Joiners such as U+200D
/// stay because some scripts and emoji need them to be spelled correctly.
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Windows drops trailing dots and spaces when it creates a file, and trailing
/// whitespace of any kind is invisible, so neither may end a name.
fn without_trailing_filler(name: &str) -> &str {
    name.trim_end_matches(|c: char| c == '.' || c.is_whitespace())
}

/// A file name split at its extension: `Holiday clip` and `mp4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameParts<'a> {
    pub stem: &'a str,
    /// `None` when nothing after a dot looks like an extension: `Mr. Smith
    /// goes home` and `.gitignore` are all stem.
    pub extension: Option<&'a str>,
}

impl<'a> NameParts<'a> {
    /// Splits at the last dot when what follows is 1 to 16 letters or digits
    /// and something comes before it; otherwise the whole name is the stem.
    /// Shortening a name and renaming a file both keep this extension.
    pub fn of(name: &'a str) -> Self {
        let split = name.rsplit_once('.').filter(|(stem, extension)| {
            !stem.is_empty()
                && !extension.is_empty()
                && extension.chars().count() <= MAX_EXTENSION_CHARS
                && extension.chars().all(char::is_alphanumeric)
        });
        match split {
            Some((stem, extension)) => Self {
                stem,
                extension: Some(extension),
            },
            None => Self {
                stem: name,
                extension: None,
            },
        }
    }
}

/// A file name the user chose without its extension. A rename never changes
/// the kind of file: the stem takes the extension of the file it names, and
/// for an unfinished download that is only known once the file is reserved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ChosenStem(String);

impl ChosenStem {
    /// The stem someone typed, unless it cannot name a file: blank or only
    /// dots. Characters a file name cannot hold are replaced, as for any
    /// typed name; a typed `.mkv` stays part of the stem.
    pub fn from_typed(raw: &str) -> Option<Self> {
        usable_file_name(raw.trim()).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `name` with this stem in place of its own, keeping its extension.
    pub fn named_like(&self, name: &str) -> FileName {
        let joined = match NameParts::of(name).extension {
            Some(extension) => format!("{}.{extension}", self.0),
            None => self.0.clone(),
        };
        FileName::new(&joined, FileNameSource::User)
    }
}

impl TryFrom<String> for ChosenStem {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::from_typed(&raw).ok_or_else(|| format!("{raw:?} cannot name a file"))
    }
}

impl From<ChosenStem> for String {
    fn from(stem: ChosenStem) -> Self {
        stem.0
    }
}

/// Tidies the end of a name and cuts it to `MAX_NAME_CHARS`, keeping its
/// extension so the file still opens with the right program.
fn shorten(name: &str) -> String {
    let name = without_trailing_filler(name);
    if name.chars().count() <= MAX_NAME_CHARS {
        return name.to_owned();
    }
    let NameParts {
        stem,
        extension: Some(extension),
    } = NameParts::of(name)
    else {
        let cut: String = name.chars().take(MAX_NAME_CHARS).collect();
        return without_trailing_filler(&cut).to_owned();
    };
    let stem_chars = MAX_NAME_CHARS - extension.chars().count() - 1;
    let cut: String = stem.chars().take(stem_chars).collect();
    format!("{}.{extension}", without_trailing_filler(&cut))
}

fn replace_unsafe_chars(name: &str) -> String {
    name.chars()
        .filter(|c| !is_bidi_control(*c))
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Windows reserves these names whatever the extension: `CON.txt` is the console.
fn is_reserved_device_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
}

/// Makes a name safe on Windows, macOS, and Linux file systems.
pub fn sanitize_file_name(raw: &str) -> String {
    usable_file_name(raw).unwrap_or_else(|| FALLBACK_NAME.to_owned())
}

/// The safe form of `raw`, or `None` when none of it can name a file.
fn usable_file_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned = replace_unsafe_chars(base);
    let name = shorten(cleaned.trim_start());
    if name.is_empty() {
        return None;
    }
    // Checked after the cut, which can reveal a reserved name. Shortened again
    // because the prefix can push a full-length name over the limit.
    Some(if is_reserved_device_name(&name) {
        shorten(&format!("_{name}"))
    } else {
        name
    })
}

/// Extracts the file name from a `Content-Disposition` header, preferring the
/// RFC 6266 `filename*` form over the legacy `filename` parameter. A real name
/// always beats a blank one. If only blank names are present one comes back
/// as sent, and `None` means the header has no name parameter at all.
pub fn file_name_from_content_disposition(header: &str) -> Option<String> {
    let mut legacy = None;
    for parameter in split_parameters(header) {
        let Some((key, value)) = parameter.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "filename*" {
            match decode_extended_value(value) {
                Some(decoded) if names_a_file(&decoded) => return Some(decoded),
                Some(blank) => {
                    legacy.get_or_insert(blank);
                }
                None => {}
            }
        } else if key == "filename" {
            let name = unquote(value);
            if names_a_file(&name) || legacy.is_none() {
                legacy = Some(name);
            }
        }
    }
    legacy
}

fn names_a_file(raw: &str) -> bool {
    usable_file_name(&decode_web_escapes(raw)).is_some()
}

fn split_parameters(header: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (index, c) in header.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                parts.push(&header[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&header[start..]);
    parts
}

fn unquote(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    inner.replace("\\\"", "\"")
}

/// Decodes `charset'language'percent-encoded` (RFC 8187). Only UTF-8 and
/// ISO-8859-1 are defined by the RFC; other charsets are rejected.
fn decode_extended_value(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?.to_ascii_lowercase();
    let _language = parts.next()?;
    let encoded = parts.next()?;
    let bytes: Vec<u8> = percent_decode_str(encoded).collect();
    let decoded = match charset.as_str() {
        "utf-8" => String::from_utf8(bytes).ok()?,
        "iso-8859-1" => bytes.into_iter().map(char::from).collect(),
        _ => return None,
    };
    Some(decoded)
}

/// Returns `dir/name`, or `dir/name (n).ext` for the first `n` for which
/// `is_taken` is false.
pub fn first_free_path(dir: &Path, name: &str, is_taken: impl Fn(&Path) -> bool) -> PathBuf {
    let candidate = dir.join(name);
    if !is_taken(&candidate) {
        return candidate;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    (1u32..)
        .map(|n| match extension {
            Some(extension) => dir.join(format!("{stem} ({n}).{extension}")),
            None => dir.join(format!("{stem} ({n})")),
        })
        .find(|path| !is_taken(path))
        .expect("an unbounded counter always finds a free name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_reported_video_name_is_clean_from_every_source() {
        let raw = "EPORNER.COM%20-%20[IJqcu4McSFM]%20Hot%20Soccer%20Mommy%20Tries%20Some%20Buttsex%20(1080).mp4";
        let clean = "EPORNER.COM - [IJqcu4McSFM] Hot Soccer Mommy Tries Some Buttsex (1080).mp4";
        let link = url::Url::parse(&format!("https://cdn.example/dl/{raw}")).unwrap();
        let header = format!("attachment; filename=\"{raw}\"");
        let server_name = file_name_from_content_disposition(&header).unwrap();

        assert_eq!(FileName::from_browser(raw).unwrap().value, clean);
        assert_eq!(FileName::from_server(&server_name).unwrap().value, clean);
        assert_eq!(FileName::from_url_path(&link).unwrap().value, clean);
    }

    /// Pieces that interact: escapes split apart, whitespace and dots that are
    /// trimmed, reserved names, and runs long enough to reach the cut.
    fn hostile_name() -> impl Strategy<Value = String> {
        let pieces: Vec<String> = [
            "%", "25", "20", "%E2", "%80", "%AE", "%E9", "&amp", "&#x202E;", ";", "\u{202E}",
            "\u{3000}", "\u{2003}", "\u{a0}", ".", "/", "\\", " ", "CON.", "COM1", "a", ".mp4",
            ".html",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(["a", ".", " ", "\u{3000}", "%25"].map(|run| run.repeat(120)))
        .collect();
        prop::collection::vec(prop::sample::select(pieces), 0..12).prop_map(|parts| parts.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        #[test]
        fn a_cleaned_name_settles_within_the_limit(raw in hostile_name()) {
            for source in [FileNameSource::Browser, FileNameSource::User] {
                let once = FileName::new(&raw, source);
                prop_assert_eq!(&once, &FileName::new(&once.value, source));
                prop_assert!(once.value.chars().count() <= MAX_NAME_CHARS);
            }
        }

        #[test]
        fn a_browser_name_settles_within_the_limit(raw in hostile_name()) {
            if let Some(once) = FileName::from_browser(&raw) {
                prop_assert_eq!(Some(once.clone()), FileName::from_browser(&once.value));
                prop_assert!(once.value.chars().count() <= MAX_NAME_CHARS);
            }
        }
    }

    #[test]
    fn trailing_unicode_whitespace_is_trimmed() {
        assert_eq!(sanitize_file_name("movie\u{3000}."), "movie");
        let at_the_cut = format!("{}\u{3000}{}", "a".repeat(199), "b".repeat(50));
        assert_eq!(sanitize_file_name(&at_the_cut), "a".repeat(199));
        let escaped = FileName::new("movie%E2%80%80.mp4.%E2%80%80", FileNameSource::Browser);
        assert_eq!(escaped.value, "movie\u{2000}.mp4");
    }

    #[test]
    fn a_reserved_name_stays_within_the_limit() {
        let plain = sanitize_file_name(&format!("CON.{}", "a".repeat(196)));
        assert_eq!(plain, format!("_CON.{}", "a".repeat(195)));
        let with_extension = sanitize_file_name(&format!("CON.{}.mp4", "a".repeat(192)));
        assert_eq!(with_extension, format!("_CON.{}.mp4", "a".repeat(191)));
    }

    #[test]
    fn very_long_web_names_keep_their_end() {
        let source = FileNameSource::Browser;
        let long_stem = FileName::new(&format!("{}.mp4", "a".repeat(5_000)), source);
        assert_eq!(long_stem.value, format!("{}.mp4", "a".repeat(196)));
        let many_folders = FileName::new(&format!("{}movie.mp4", "d/".repeat(3_000)), source);
        assert_eq!(many_folders.value, "movie.mp4");
        let nested = FileName::new(&format!("%{}20x.mp4", "25".repeat(5_000)), source);
        assert!(nested.value.ends_with(".mp4"), "{}", nested.value);
    }

    #[test]
    fn url_guess_decodes_last_segment() {
        let url = Url::parse("https://example.com/files/My%20Report.pdf?x=1").unwrap();
        assert_eq!(FileName::guess_from_url(&url).value, "My Report.pdf");

        let root = Url::parse("https://example.com/").unwrap();
        assert_eq!(FileName::guess_from_url(&root).value, FALLBACK_NAME);
    }

    #[test]
    fn web_names_lose_url_and_html_escapes() {
        let cases = [
            ("My%20Video.mp4", "My Video.mp4"),
            ("My%2520Video.mp4", "My Video.mp4"),
            ("Tom &amp; Jerry.mp4", "Tom & Jerry.mp4"),
            ("Tom%20%26amp%3B%20Jerry.mp4", "Tom & Jerry.mp4"),
            ("It&#39;s&nbsp;here.mp4", "It's here.mp4"),
            ("na%C3%AFve.mp4", "na\u{ef}ve.mp4"),
        ];
        for source in [
            FileNameSource::Browser,
            FileNameSource::Server,
            FileNameSource::Guess,
        ] {
            for (raw, expected) in cases {
                assert_eq!(FileName::new(raw, source).value, expected, "{raw}");
            }
        }
    }

    #[test]
    fn plain_percent_and_ampersand_survive_decoding() {
        let cases = [
            ("100% real.mp4", "100% real.mp4"),
            ("Rock&Roll.mp4", "Rock&Roll.mp4"),
            ("caf%E9.mp4", "caf%E9.mp4"),
            ("caf%E9%20menu.mp4", "caf%E9 menu.mp4"),
        ];
        for (raw, expected) in cases {
            let name = FileName::new(raw, FileNameSource::Browser);
            assert_eq!(name.value, expected);
        }
    }

    #[test]
    fn cleaning_a_clean_name_changes_nothing() {
        let deeply_nested = format!("%{}20x.mp4", "25".repeat(5_000));
        let entity_at_the_cut = format!("{}&amp{}.mp4", "a".repeat(191), "b".repeat(50));
        let raws = [
            "%\u{202E}20x.mp4",
            entity_at_the_cut.as_str(),
            "%25252520x.mp4",
            "%2525252525252520x.mp4",
            "caf%e9%20menu.mp4",
            "a&amp;amp;amp;amp;amp;b.mp4",
            "100%2525 real.mp4",
            deeply_nested.as_str(),
        ];
        for raw in raws {
            let once = FileName::new(raw, FileNameSource::Browser);
            let twice = FileName::new(&once.value, FileNameSource::Browser);
            assert_eq!(once, twice, "{raw}");
        }
    }

    #[test]
    fn bidi_controls_cannot_disguise_an_extension() {
        assert_eq!(sanitize_file_name("photo\u{202E}gpj.exe"), "photogpj.exe");
        let escapes = ["photo%E2%80%AEgpj.exe", "photo&#x202E;gpj.exe"];
        for raw in escapes {
            let name = FileName::new(raw, FileNameSource::Browser);
            assert_eq!(name.value, "photogpj.exe", "{raw}");
        }
        assert_eq!(FileName::from_browser("&#x202E;"), None);
    }

    #[test]
    fn joiners_stay_because_scripts_need_them() {
        assert_eq!(sanitize_file_name("a\u{200D}b.mp4"), "a\u{200D}b.mp4");
    }

    #[test]
    fn long_names_keep_their_extension() {
        let shortened = sanitize_file_name(&format!("{}.mp4", "a".repeat(300)));
        assert_eq!(shortened, format!("{}.mp4", "a".repeat(MAX_NAME_CHARS - 4)));
    }

    #[test]
    fn a_name_at_the_limit_is_untouched() {
        let at_limit = format!("{}.mp4", "a".repeat(MAX_NAME_CHARS - 4));
        assert_eq!(sanitize_file_name(&at_limit), at_limit);
    }

    #[test]
    fn the_cut_tidies_the_end_of_the_stem() {
        let stem_chars = MAX_NAME_CHARS - 4;
        let raw = format!("{} . {}.mp4", "a".repeat(stem_chars - 3), "b".repeat(50));
        assert_eq!(
            sanitize_file_name(&raw),
            format!("{}.mp4", "a".repeat(stem_chars - 3))
        );
    }

    #[test]
    fn a_name_splits_at_its_extension() {
        let parts = |name| {
            let NameParts { stem, extension } = NameParts::of(name);
            (stem, extension)
        };
        assert_eq!(parts("Holiday clip.mp4"), ("Holiday clip", Some("mp4")));
        assert_eq!(parts("backup.tar.gz"), ("backup.tar", Some("gz")));
        assert_eq!(parts("Mr. Smith goes home"), ("Mr. Smith goes home", None));
        assert_eq!(parts(".gitignore"), (".gitignore", None));
        assert_eq!(parts("readme"), ("readme", None));
    }

    #[test]
    fn a_rename_keeps_the_extension() {
        let renamed = |current, typed| {
            ChosenStem::from_typed(typed).map(|stem| stem.named_like(current).value)
        };
        assert_eq!(renamed("clip.mp4", "Holiday"), Some("Holiday.mp4".into()));
        // A typed extension is part of the stem; the file's own stays.
        assert_eq!(
            renamed("clip.mp4", "Holiday.mkv"),
            Some("Holiday.mkv.mp4".into())
        );
        assert_eq!(
            renamed("clip.mp4", " Holiday. "),
            Some("Holiday.mp4".into())
        );
        assert_eq!(renamed("readme", "notes"), Some("notes".into()));
        assert_eq!(renamed("clip.mp4", "  "), None);
        assert_eq!(renamed("clip.mp4", "..."), None);
    }

    #[test]
    fn a_name_typed_over_an_offered_one_keeps_its_extension() {
        let typed_over = |typed, offered| {
            FileName::new(offered, FileNameSource::Browser)
                .typed_over(typed)
                .map(|n| n.value)
        };
        assert_eq!(typed_over("hello", "adasf.mp4"), Some("hello.mp4".into()));
        assert_eq!(
            typed_over("hello.mp4", "adasf.mp4"),
            Some("hello.mp4".into())
        );
        assert_eq!(
            typed_over(" hello.MP4 ", "adasf.mp4"),
            Some("hello.mp4".into())
        );
        assert_eq!(
            typed_over("hello.mkv", "adasf.mp4"),
            Some("hello.mkv.mp4".into())
        );
        assert_eq!(typed_over("v1.2", "adasf.mp4"), Some("v1.2.mp4".into()));
        assert_eq!(typed_over("hello.txt", "readme"), Some("hello.txt".into()));
        assert_eq!(typed_over(".mp4", "adasf.mp4"), Some(".mp4.mp4".into()));
        assert_eq!(typed_over("  ", "adasf.mp4"), None);
        let typed = FileName::new("adasf.mp4", FileNameSource::Browser)
            .typed_over("hello")
            .unwrap();
        assert_eq!(typed.source, FileNameSource::User);
    }

    #[test]
    fn a_typed_extension_replaces_one_guessed_from_the_address() {
        let guessed = FileName::new("download.php", FileNameSource::Guess);
        let typed_over = |typed| guessed.typed_over(typed).map(|n| n.value);
        assert_eq!(typed_over("report.pdf"), Some("report.pdf".into()));
        assert_eq!(typed_over("report"), Some("report.php".into()));
        assert_eq!(typed_over("report.PHP"), Some("report.php".into()));
    }

    #[test]
    fn a_saved_stem_is_checked_when_read() {
        let stem: ChosenStem = serde_json::from_str(r#""Holiday""#).unwrap();
        assert_eq!(serde_json::to_string(&stem).unwrap(), r#""Holiday""#);
        assert!(serde_json::from_str::<ChosenStem>(r#""  ""#).is_err());
    }

    #[test]
    fn text_after_a_dot_is_not_an_extension() {
        let cases = [
            "a".repeat(300),
            format!(
                "{}.{}",
                "a".repeat(250),
                "b".repeat(MAX_EXTENSION_CHARS + 1)
            ),
            format!("Mr. Long {} title", "a".repeat(300)),
            format!("{}. two words", "a".repeat(300)),
        ];
        for raw in cases {
            let expected: String = raw.chars().take(MAX_NAME_CHARS).collect();
            assert_eq!(sanitize_file_name(&raw), expected);
        }
    }

    #[test]
    fn a_browser_name_of_only_escapes_names_nothing() {
        for raw in ["", "   ", "%20", "&nbsp;", "&#47;", "..%2F"] {
            assert_eq!(FileName::from_browser(raw), None, "{raw}");
        }
        let named = FileName::from_browser("My%20Movie.mp4").unwrap();
        assert_eq!(named.value, "My Movie.mp4");
        assert_eq!(named.source, FileNameSource::Browser);
    }

    #[test]
    fn decoded_names_are_still_sanitized() {
        let source = FileNameSource::Server;
        assert_eq!(FileName::new("..%2F..%2Fpasswd", source).value, "passwd");
        assert_eq!(FileName::new("..&#47;passwd", source).value, "passwd");
        assert_eq!(FileName::new("a%3Cb%00.mp4", source).value, "a_b_.mp4");
    }

    #[test]
    fn typed_names_stay_literal() {
        let name = FileName::new("50%20off &amp; more.mp4", FileNameSource::User);
        assert_eq!(name.value, "50%20off &amp; more.mp4");
    }

    #[test]
    fn stream_guess_decodes_folder_name() {
        let url = Url::parse("https://cdn.example/my%20scene/master.m3u8").unwrap();
        assert_eq!(FileName::guess_for_stream(&url).value, "my scene");
    }

    #[test]
    fn stream_names_skip_generic_segments() {
        let named =
            Url::parse("https://cdn.example/videos/my-scene/1080p/master.m3u8?t=1").unwrap();
        assert_eq!(FileName::guess_for_stream(&named).value, "my-scene");
        let bare = Url::parse("https://cdn.example/123/index.m3u8").unwrap();
        assert_eq!(FileName::guess_for_stream(&bare).value, "video");
    }

    #[test]
    fn sanitizes_hostile_names() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("a<b>c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_file_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_file_name("COM1"), "_COM1");
        assert_eq!(sanitize_file_name("trailing. "), "trailing");
        assert_eq!(sanitize_file_name(""), FALLBACK_NAME);
    }

    #[test]
    fn content_disposition_prefers_extended_name() {
        let header =
            r#"attachment; filename="fallback.txt"; filename*=UTF-8''na%C3%AFve%20file.txt"#;
        assert_eq!(
            file_name_from_content_disposition(header).as_deref(),
            Some("naïve file.txt")
        );
    }

    #[test]
    fn a_blank_server_name_names_nothing() {
        for raw in ["", "   ", "%20", "&nbsp;", "&#47;", "..."] {
            assert_eq!(FileName::from_server(raw), None, "{raw:?}");
        }
        let named = FileName::from_server("My%20Clip.mp4").unwrap();
        assert_eq!(named.value, "My Clip.mp4");
        assert_eq!(named.source, FileNameSource::Server);
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn a_typed_name_is_literal_unless_it_names_nothing() {
        let typed = FileName::from_typed("50%20off.mp4").unwrap();
        assert_eq!(typed.value, "50%20off.mp4");
        assert_eq!(typed.source, FileNameSource::User);
        for nothing in ["", "   ", "...", "a/"] {
            assert_eq!(FileName::from_typed(nothing), None, "{nothing:?}");
        }
    }

    #[test]
    fn a_reserved_target_gives_the_name_and_keeps_the_source() {
        let guess = FileName::guess_from_url(&url("https://cdn.example/clip.mp4"));
        let saved = guess.saved_as(Path::new("downloads").join("clip (1).mp4").as_path());
        assert_eq!(saved.value, "clip (1).mp4");
        assert_eq!(saved.source, guess.source);
    }

    #[test]
    fn a_chosen_name_is_never_replaced() {
        let elsewhere = url("https://cdn.example/other.mp4");
        for source in [FileNameSource::User, FileNameSource::Browser] {
            let chosen = FileName::new("mine.mp4", source);
            for header in [Some("real.mp4"), Some(""), None] {
                assert_eq!(chosen.after_response(header, &elsewhere), chosen);
            }
        }
    }

    #[test]
    fn a_real_server_name_replaces_any_guess() {
        let final_url = url("https://cdn.example/other.mp4");
        for page in ["https://cdn.example/clip.mp4", "https://cdn.example/"] {
            let guess = FileName::guess_from_url(&url(page));
            let named = guess.after_response(Some("Real%20Name.mp4"), &final_url);
            assert_eq!(named.value, "Real Name.mp4");
            assert_eq!(named.source, FileNameSource::Server);
        }
    }

    #[test]
    fn a_blank_or_missing_server_name_keeps_a_real_guess() {
        let final_url = url("https://cdn.example/opaque");
        let pages = [
            "https://cdn.example/clip.mp4",
            // A name, even though it matches the fallback's spelling.
            "https://cdn.example/download",
        ];
        for page in pages {
            let guess = FileName::guess_from_url(&url(page));
            for header in [Some(""), Some("%20"), Some("..."), None] {
                assert_eq!(guess.after_response(header, &final_url), guess, "{page}");
            }
        }
    }

    #[test]
    fn with_no_guess_the_url_redirects_ended_at_names_the_file() {
        let guess = FileName::guess_from_url(&url("https://h.example/"));
        let final_url = url("https://cdn.example/clip.mp4");
        for header in [Some(""), None] {
            assert_eq!(guess.after_response(header, &final_url).value, "clip.mp4");
        }
    }

    #[test]
    fn a_blank_server_name_with_no_name_anywhere_becomes_unnamed() {
        let bare = url("https://h.example/");
        let guess = FileName::guess_from_url(&bare);
        let named = guess.after_response(Some("%20"), &bare);
        assert_eq!(named.value, BLANK_SERVER_NAME);
    }

    #[test]
    fn with_no_name_anywhere_and_no_header_the_placeholder_stays() {
        let bare = url("https://h.example/");
        let guess = FileName::guess_from_url(&bare);
        assert_eq!(guess.after_response(None, &bare).value, FALLBACK_NAME);
    }

    #[test]
    fn names_saved_before_the_guess_source_was_renamed_still_load() {
        let saved = r#"{"value":"clip.mp4","source":"url"}"#;
        let name: FileName = serde_json::from_str(saved).unwrap();
        assert_eq!(name.source, FileNameSource::Guess);
    }

    #[test]
    fn only_a_url_with_a_name_in_its_path_offers_one() {
        let named = Url::parse("https://cdn.example/My%20Clip.mp4").unwrap();
        assert_eq!(
            FileName::from_url_path(&named).unwrap().value,
            "My Clip.mp4"
        );
        let bare = Url::parse("https://cdn.example/").unwrap();
        assert_eq!(FileName::from_url_path(&bare), None);
    }

    #[test]
    fn content_disposition_reports_a_blank_name() {
        let blank = [
            r#"attachment; filename="""#,
            "attachment; filename=",
            "attachment; filename*=UTF-8''",
        ];
        for header in blank {
            assert_eq!(
                file_name_from_content_disposition(header).as_deref(),
                Some(""),
                "{header}"
            );
        }
    }

    #[test]
    fn content_disposition_prefers_a_real_name_over_a_blank_extended_one() {
        let orders = [
            r#"attachment; filename*=UTF-8''; filename="good.mp4""#,
            r#"attachment; filename="good.mp4"; filename*=UTF-8''"#,
            r#"attachment; filename*=UTF-8''%20; filename="good.mp4""#,
            r#"attachment; filename="good.mp4"; filename*=UTF-8''%20"#,
            r#"attachment; filename*=UTF-8''..%2F; filename="good.mp4""#,
            r#"attachment; filename*=UTF-8''...; filename="good.mp4""#,
            r#"attachment; filename="good.mp4"; filename="""#,
            r#"attachment; filename=""; filename="good.mp4""#,
        ];
        for header in orders {
            assert_eq!(
                file_name_from_content_disposition(header).as_deref(),
                Some("good.mp4"),
                "{header}"
            );
        }
    }

    #[test]
    fn content_disposition_legacy_name_with_semicolon() {
        let header = r#"attachment; filename="a;b.zip""#;
        assert_eq!(
            file_name_from_content_disposition(header).as_deref(),
            Some("a;b.zip")
        );
        assert_eq!(file_name_from_content_disposition("inline"), None);
    }

    #[test]
    fn finds_numbered_free_path() {
        let dir = Path::new("downloads");
        let taken = [dir.join("a.zip"), dir.join("a (1).zip")];
        let path = first_free_path(dir, "a.zip", |p| taken.iter().any(|t| t == p));
        assert_eq!(path, dir.join("a (2).zip"));
        assert_eq!(first_free_path(dir, "b", |_| false), dir.join("b"));
    }
}
