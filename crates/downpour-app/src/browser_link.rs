//! A download link handed over by the browser, and what follows from it.
//!
//! The link is read once where it arrives: its address, the page it was
//! found on, whether it is a video stream, and the request headers. The
//! file-info dialog, the duplicate check, and auto-start all read these
//! facts from here instead of working them out again.

use std::path::PathBuf;

use downpour_core::category::{CategoryId, VIDEO_EXTENSIONS};
use downpour_core::history::extension_of;
use downpour_core::stream::{StreamFormat, VideoQuality};
use downpour_core::{
    Categories, DownloadKind, EngineSettings, FileName, NewDownload, QueueId, RequestContext,
    StartMode,
};
use downpour_protocol::messages::{BrowserDownload, QualityPick, StreamHint};
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserLink {
    item: BrowserDownload,
    url: Url,
    page: Option<Url>,
    stream: Option<StreamFormat>,
}

impl BrowserLink {
    pub fn parse(item: BrowserDownload) -> Result<Self, url::ParseError> {
        let url = Url::parse(&item.url)?;
        // The page the extension reported, or the Referer for links captured
        // before it reported pages; only web pages count.
        let page = item
            .page_url
            .as_deref()
            .or(item.referrer.as_deref())
            .and_then(|page| Url::parse(page).ok())
            .filter(|page| matches!(page.scheme(), "http" | "https"));
        let stream = match item.stream {
            Some(StreamHint::Hls) => Some(StreamFormat::Hls),
            Some(StreamHint::Dash) => Some(StreamFormat::Dash),
            None => StreamFormat::from_url(&url),
        };
        Ok(Self {
            item,
            url,
            page,
            stream,
        })
    }

    /// The size the browser reported, if any.
    pub fn size(&self) -> Option<u64> {
        self.item.size
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn page(&self) -> Option<&Url> {
        self.page.as_ref()
    }

    /// `Some` when the link is a video stream's manifest.
    pub fn stream(&self) -> Option<StreamFormat> {
        self.stream
    }

    /// The quality picked on the page's video button, if any.
    pub fn picked_quality(&self) -> Option<VideoQuality> {
        self.item.quality.map(|pick| match pick {
            QualityPick::Best => VideoQuality::Best,
            QualityPick::MaxHeight { pixels } => VideoQuality::MaxHeight { pixels },
        })
    }

    /// The headers the page's own requests carried.
    pub fn context(&self) -> RequestContext {
        RequestContext {
            referrer: self.item.referrer.clone(),
            user_agent: self.item.user_agent.clone(),
            cookies: self.item.cookies.clone(),
            origin: self.item.origin.clone(),
            accept_language: self.item.accept_language.clone(),
        }
    }

    /// The name the link itself brings: a video is named after the page's
    /// title (a far better name than `master.m3u8` or `v_1080_3f2a.mp4`),
    /// another file as the browser named it, and a name someone typed beats
    /// both. `None` leaves the name to the engine, and to the server.
    pub fn carried_name(&self) -> Option<FileName> {
        let typed = self.item.typed_name.as_deref();
        if let Some(name) = typed.and_then(FileName::from_typed) {
            return Some(name);
        }
        match self.stream {
            Some(_) => self.item.title.as_deref().and_then(FileName::from_browser),
            None => self
                .item
                .video_title
                .as_deref()
                .zip(self.video_extension())
                .and_then(|(title, extension)| titled_video(title, &extension))
                .or_else(|| {
                    self.item
                        .file_name
                        .as_deref()
                        .and_then(FileName::from_browser)
                }),
        }
    }

    /// The extension of the video file this link is, or `None` for any
    /// other file. The first extension the browser's name or the address has
    /// decides, so a `.zip` the browser named is never taken for a video
    /// because its address ends in `.mp4`. The type says the rest: what a
    /// file without an extension is, and whether a `.ts` is a video.
    fn video_extension(&self) -> Option<String> {
        let browser_name = self
            .item
            .file_name
            .as_deref()
            .and_then(FileName::from_browser);
        let named = [browser_name, FileName::from_url_path(&self.url)]
            .into_iter()
            .flatten()
            .map(|name| extension_of(&name.value))
            .find(|extension| !extension.is_empty());
        let typed = self.item.mime.as_deref().and_then(|mime| {
            VIDEO_MIME_EXTENSIONS
                .iter()
                .find(|(known, _)| mime.eq_ignore_ascii_case(known))
                .map(|(_, extension)| *extension)
        });
        match named {
            Some(extension) if VIDEO_EXTENSIONS.contains(&extension.as_str()) => Some(extension),
            Some(extension) => typed.filter(|typed| *typed == extension).map(str::to_owned),
            None => typed.map(str::to_owned),
        }
    }

    /// The name to offer in the file-info dialog.
    pub fn suggested_name(&self) -> String {
        self.carried_name()
            .unwrap_or_else(|| match self.stream {
                Some(_) => FileName::guess_for_stream(&self.url),
                None => FileName::guess_from_url(&self.url),
            })
            .value
    }

    /// What to request given the dialog's name box: anything but the offered
    /// name is the user's own, kept literally. An empty box means no opinion.
    pub fn requested_name(&self, typed: &str) -> Option<FileName> {
        FileName::from_typed(typed)
            .filter(|name| name.value != self.suggested_name())
            .or_else(|| self.carried_name())
    }

    /// The category a file named `name` falls in; streams become `.mp4`
    /// videos whatever the manifest's own extension.
    pub fn category_for(&self, name: &str, categories: &Categories) -> CategoryId {
        match self.stream {
            Some(_) => categories.classify(&format!("{name}.mp4")),
            None => categories.classify(name),
        }
    }
}

/// The extension of a video served without one in its name.
const VIDEO_MIME_EXTENSIONS: &[(&str, &str)] = &[
    ("video/mp4", "mp4"),
    ("video/webm", "webm"),
    ("video/x-matroska", "mkv"),
    ("video/quicktime", "mov"),
    ("video/x-flv", "flv"),
    // `.ts` alone may be TypeScript; the type says it is a video.
    ("video/mp2t", "ts"),
];

/// `title.extension`, unless the title already ends in that extension: some
/// pages are titled after the file itself.
fn titled_video(title: &str, extension: &str) -> Option<FileName> {
    let title = title.trim();
    let suffix = format!(".{extension}");
    let stem = title
        .len()
        .checked_sub(suffix.len())
        .filter(|&cut| cut > 0 && title.is_char_boundary(cut))
        .filter(|&cut| title[cut..].eq_ignore_ascii_case(&suffix))
        .map_or(title, |cut| &title[..cut]);
    let names_nothing = FileName::from_browser(stem).is_none();
    if names_nothing {
        // "" or "%20" would leave just ".mp4".
        return None;
    }
    FileName::from_browser(&format!("{stem}{suffix}"))
}

/// Where a download of `category` is saved by default.
pub fn default_folder(settings: &EngineSettings, category: CategoryId) -> PathBuf {
    if settings.organize_by_category {
        settings
            .categories
            .get(category)
            .save_folder(&settings.download_dir)
    } else {
        settings.download_dir.clone()
    }
}

/// What the file-info dialog would add with its defaults untouched: the name
/// the link carries (or none, so the engine and server decide), the category's
/// folder, the Main queue, and the quality picked on the page. Without a pick the engine applies the quality from
/// Settings when the download starts.
pub fn request_without_asking(link: BrowserLink, settings: &EngineSettings) -> NewDownload {
    // A name the server may still improve on stays unset.
    let file_name = link.carried_name();
    // Without a name the engine picks the category folder once the file has
    // been named.
    let folder = file_name.as_ref().map(|name| {
        let category = link.category_for(&name.value, &settings.categories);
        downpour_core::paths::native(&default_folder(settings, category))
    });
    let kind = match link.stream {
        Some(format) => DownloadKind::Stream {
            format,
            quality: link.picked_quality(),
        },
        None => DownloadKind::Detect,
    };
    NewDownload {
        context: link.context(),
        url: link.url,
        file_name,
        folder,
        start: StartMode::Queue,
        kind,
        queue: Some(QueueId::MAIN),
        page_url: link.page,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use downpour_core::FileNameSource;

    pub fn item(url: &str) -> BrowserDownload {
        BrowserDownload {
            url: url.into(),
            file_name: None,
            typed_name: None,
            referrer: None,
            cookies: None,
            user_agent: None,
            mime: None,
            size: None,
            stream: None,
            quality: None,
            title: None,
            video_title: None,
            page_url: None,
            origin: None,
            accept_language: None,
        }
    }

    pub fn link(url: &str) -> BrowserLink {
        BrowserLink::parse(item(url)).unwrap()
    }

    fn stream_item() -> BrowserDownload {
        BrowserDownload {
            referrer: Some("https://site.example/watch/3".into()),
            user_agent: Some("UA".into()),
            stream: Some(StreamHint::Hls),
            title: Some("Scene 3: The Return".into()),
            page_url: Some("https://site.example/watch/3".into()),
            origin: Some("https://site.example".into()),
            accept_language: Some("en-US".into()),
            ..item("https://cdn.example/v/master.m3u8")
        }
    }

    #[test]
    fn the_extension_sends_qualities_in_the_engines_own_json() {
        for quality in VideoQuality::CHOICES {
            let json = serde_json::to_string(&quality).unwrap();
            let pick: QualityPick = serde_json::from_str(&json).unwrap();
            let link = BrowserLink::parse(BrowserDownload {
                quality: Some(pick),
                ..item("https://cdn.example/a.m3u8")
            })
            .unwrap();
            assert_eq!(link.picked_quality(), Some(quality), "{json}");
        }
    }

    #[test]
    fn the_page_is_the_reported_one_or_the_referrer_and_only_a_web_page() {
        let reported = BrowserLink::parse(BrowserDownload {
            page_url: Some("https://site.example/watch".into()),
            referrer: Some("https://other.example/".into()),
            ..item("https://cdn.example/a.zip")
        })
        .unwrap();
        assert_eq!(
            reported.page().unwrap().as_str(),
            "https://site.example/watch"
        );
        let referred = BrowserLink::parse(BrowserDownload {
            referrer: Some("https://other.example/".into()),
            ..item("https://cdn.example/a.zip")
        })
        .unwrap();
        assert_eq!(referred.page().unwrap().as_str(), "https://other.example/");
        let local = BrowserLink::parse(BrowserDownload {
            page_url: Some("file:///C:/page.html".into()),
            ..item("https://cdn.example/a.zip")
        })
        .unwrap();
        assert_eq!(local.page(), None);
    }

    fn video_on_page(url: &str, title: &str) -> BrowserDownload {
        BrowserDownload {
            video_title: Some(title.into()),
            ..item(url)
        }
    }

    #[test]
    fn an_older_add_ons_page_title_never_names_a_file() {
        // Its `title` named streams only, even when sent with a listed link.
        let listed = BrowserLink::parse(BrowserDownload {
            title: Some("Season 1".into()),
            ..item("https://site.example/ep1.mp4")
        })
        .unwrap();
        assert_eq!(listed.suggested_name(), "ep1.mp4");
    }

    #[test]
    fn a_video_file_is_named_after_its_page() {
        let named = |download| BrowserLink::parse(download).unwrap().suggested_name();
        assert_eq!(
            named(video_on_page(
                "https://cdn.example/v_3f2a.mp4",
                "Holiday: Rome"
            )),
            "Holiday_ Rome.mp4"
        );
        // The browser's name gives the extension when the address has none.
        assert_eq!(
            named(BrowserDownload {
                file_name: Some("v_3f2a.webm".into()),
                ..video_on_page("https://cdn.example/get?id=7", "Rome")
            }),
            "Rome.webm"
        );
        assert_eq!(
            named(BrowserDownload {
                mime: Some("video/mp4".into()),
                ..video_on_page("https://cdn.example/get?id=7", "Rome")
            }),
            "Rome.mp4"
        );
        // An extensionless browser name leaves the extension to the address.
        assert_eq!(
            named(BrowserDownload {
                file_name: Some("download".into()),
                ..video_on_page("https://cdn.example/clip.mkv", "Rome")
            }),
            "Rome.mkv"
        );
        // A page titled after the file does not double the extension.
        assert_eq!(
            named(video_on_page("https://cdn.example/clip.MP4", "clip.mp4")),
            "clip.mp4"
        );
    }

    #[test]
    fn other_files_and_empty_titles_keep_the_files_own_name() {
        let named = |download| BrowserLink::parse(download).unwrap().suggested_name();
        assert_eq!(
            named(video_on_page("https://cdn.example/setup.exe", "Downloads")),
            "setup.exe"
        );
        assert_eq!(
            named(video_on_page("https://cdn.example/clip.mp4", "  ")),
            "clip.mp4"
        );
        // `.ts` may be source code; only the type makes it a video.
        assert_eq!(
            named(video_on_page(
                "https://code.example/app.ts",
                "app.ts at main"
            )),
            "app.ts"
        );
        assert_eq!(
            named(BrowserDownload {
                mime: Some("video/mp2t".into()),
                ..video_on_page("https://cdn.example/rome.ts", "Rome")
            }),
            "Rome.ts"
        );
        // The browser's own type wins over the address's.
        assert_eq!(
            named(BrowserDownload {
                file_name: Some("pack.zip".into()),
                ..video_on_page("https://cdn.example/clip.mp4", "Rome")
            }),
            "pack.zip"
        );
    }

    #[test]
    fn a_stream_is_known_from_the_hint_or_the_address() {
        assert_eq!(
            BrowserLink::parse(stream_item()).unwrap().stream(),
            Some(StreamFormat::Hls)
        );
        assert_eq!(
            link("https://cdn.example/v/manifest.mpd").stream(),
            Some(StreamFormat::Dash)
        );
        assert_eq!(link("https://cdn.example/a.zip").stream(), None);
    }

    #[test]
    fn auto_started_streams_are_named_after_the_page_and_keep_headers() {
        let request = request_without_asking(
            BrowserLink::parse(stream_item()).unwrap(),
            &EngineSettings::default(),
        );
        assert_eq!(request.file_name.unwrap().value, "Scene 3_ The Return");
        assert_eq!(
            request.kind,
            DownloadKind::Stream {
                format: StreamFormat::Hls,
                quality: None
            }
        );
        assert_eq!(
            request.context.origin.as_deref(),
            Some("https://site.example")
        );
        assert_eq!(request.context.accept_language.as_deref(), Some("en-US"));
        assert_eq!(
            request.page_url.unwrap().as_str(),
            "https://site.example/watch/3"
        );
    }

    #[test]
    fn a_quality_picked_on_the_page_goes_with_an_auto_started_stream() {
        let link = BrowserLink::parse(BrowserDownload {
            quality: Some(QualityPick::MaxHeight { pixels: 720 }),
            ..stream_item()
        })
        .unwrap();
        let request = request_without_asking(link, &EngineSettings::default());
        assert_eq!(
            request.kind,
            DownloadKind::Stream {
                format: StreamFormat::Hls,
                quality: Some(VideoQuality::MaxHeight { pixels: 720 })
            }
        );
    }

    #[test]
    fn auto_started_files_go_to_their_categorys_folder() {
        let settings = EngineSettings {
            download_dir: PathBuf::from("D:/dl"),
            ..EngineSettings::default()
        };
        let named = BrowserLink::parse(BrowserDownload {
            file_name: Some("movie.mp4".into()),
            ..item("https://cdn.example/get?id=7")
        })
        .unwrap();
        let request = request_without_asking(named, &settings);
        assert_eq!(request.file_name.unwrap().value, "movie.mp4");
        assert_eq!(
            request.folder,
            Some(downpour_core::paths::native(
                &PathBuf::from("D:/dl").join("Video")
            ))
        );
        let escaped = BrowserLink::parse(BrowserDownload {
            file_name: Some("My%20Movie.mp4".into()),
            ..item("https://cdn.example/get?id=7")
        })
        .unwrap();
        assert_eq!(escaped.suggested_name(), "My Movie.mp4");
        let typed = BrowserLink::parse(BrowserDownload {
            typed_name: Some("50%20off.mp4".into()),
            file_name: Some("browser.mp4".into()),
            ..item("https://cdn.example/get?id=7")
        })
        .unwrap();
        let name = request_without_asking(typed, &settings).file_name.unwrap();
        assert_eq!(name.value, "50%20off.mp4");
        assert_eq!(name.source, FileNameSource::User);
        // A name of nothing but escapes must not hide the URL's name, or
        // stop the server from naming the file.
        let blank = BrowserLink::parse(BrowserDownload {
            file_name: Some("%20".into()),
            ..item("https://cdn.example/clip.mp4")
        })
        .unwrap();
        assert_eq!(blank.suggested_name(), "clip.mp4");
        assert_eq!(request_without_asking(blank, &settings).file_name, None);
        // Without the browser's name, the server may still name the file,
        // and the engine picks its category folder then.
        let unnamed = request_without_asking(link("https://cdn.example/get?id=7"), &settings);
        assert_eq!(unnamed.file_name, None);
        assert_eq!(unnamed.folder, None);
        assert_eq!(unnamed.queue, Some(QueueId::MAIN));
    }
}
