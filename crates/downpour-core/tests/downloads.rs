//! End-to-end downloads through the public manager API against a local server.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use downpour_core::{
    DownloadManager, DownloadStatus, EngineSettings, FileName, FileNameSource, NewDownload,
    RenameError, Renamed, StartMode, Store,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

#[derive(Clone, Copy)]
enum RangeSupport {
    Ranges,
    NoRanges,
}

/// How the server answers a request beyond its connection limit.
#[derive(Clone, Copy)]
enum Refusal {
    TooManyRequests,
    /// Some download servers answer extra connections like this, as if
    /// the file did not exist.
    NotFound,
}

impl Refusal {
    fn response(self) -> &'static [u8] {
        match self {
            Refusal::TooManyRequests => {
                b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            }
            Refusal::NotFound => {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            }
        }
    }
}

/// How the test server behaves.
#[derive(Clone, Copy)]
struct Behavior {
    ranges: RangeSupport,
    /// Requests beyond this many in flight are refused with `refusal`.
    max_concurrent: usize,
    refusal: Refusal,
    /// After this many accepted requests, every new one is refused with
    /// `refusal`; connections already open keep streaming. Some download
    /// links behave like this some minutes into a download.
    accepted_requests: usize,
}

impl From<RangeSupport> for Behavior {
    fn from(ranges: RangeSupport) -> Self {
        Behavior {
            ranges,
            max_concurrent: usize::MAX,
            refusal: Refusal::TooManyRequests,
            accepted_requests: usize::MAX,
        }
    }
}

struct TestServer {
    url: Url,
    range_requests: Arc<AtomicUsize>,
    refused_requests: Arc<AtomicUsize>,
}

/// Minimal HTTP/1.1 server that serves `body`, honoring `Range` when asked.
/// Each response is written in small slices with a delay so downloads last
/// long enough to pause and to open several connections.
async fn serve(body: Arc<Vec<u8>>, behavior: impl Into<Behavior>) -> TestServer {
    let behavior = behavior.into();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let accepted_so_far = Arc::new(AtomicUsize::new(0));
    let refused_requests = Arc::new(AtomicUsize::new(0));
    let refused = Arc::clone(&refused_requests);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/files/sample.bin",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let range_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&range_requests);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = Arc::clone(&body);
            let counter = Arc::clone(&counter);
            let in_flight = Arc::clone(&in_flight);
            let refused = Arc::clone(&refused);
            let accepted_so_far = Arc::clone(&accepted_so_far);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let range = request
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .map(|spec| {
                        let (start, end) = spec.trim().split_once('-').unwrap();
                        let start: usize = start.parse().unwrap();
                        let end = end.parse::<usize>().map(|e| e + 1).unwrap_or(body.len());
                        (start, end.min(body.len()))
                    });
                let link_expired =
                    accepted_so_far.fetch_add(1, Ordering::SeqCst) >= behavior.accepted_requests;
                if link_expired {
                    refused.fetch_add(1, Ordering::SeqCst);
                    let _ = socket.write_all(behavior.refusal.response()).await;
                    return;
                }
                if in_flight.fetch_add(1, Ordering::SeqCst) >= behavior.max_concurrent {
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    refused.fetch_add(1, Ordering::SeqCst);
                    let _ = socket.write_all(behavior.refusal.response()).await;
                    return;
                }
                let _release = InFlight(Arc::clone(&in_flight));
                let (head, slice) = match (behavior.ranges, range) {
                    (RangeSupport::Ranges, Some((start, end))) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n",
                            end - start, start, end - 1, body.len()
                        );
                        (head, &body[start..end])
                    }
                    _ => (
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        ),
                        &body[..],
                    ),
                };
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                for piece in slice.chunks(64 * 1024) {
                    if socket.write_all(piece).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        }
    });
    TestServer {
        url,
        range_requests,
        refused_requests,
    }
}

/// Decrements the in-flight request count when a response ends.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn sample_body(len: usize) -> Arc<Vec<u8>> {
    Arc::new((0..len).map(|i| (i * 31 % 251) as u8).collect())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("downpour-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn manager_in(dir: &Path, connections: u8) -> DownloadManager {
    manager_with_saved_list(dir, connections, Vec::new())
}

fn manager_with_saved_list(
    dir: &Path,
    connections: u8,
    records: Vec<downpour_core::record::DownloadRecord>,
) -> DownloadManager {
    let settings = EngineSettings {
        download_dir: dir.join("downloads"),
        organize_by_category: false,
        connections_per_download: connections,
        ..Default::default()
    };
    let store = Store::open(dir.join("state")).unwrap();
    DownloadManager::start(store, records, settings, tokio::runtime::Handle::current()).unwrap()
}

/// A name as versions that did not clean web escapes saved it.
fn escaped_browser_name(value: &str) -> FileName {
    FileName {
        value: value.to_owned(),
        source: FileNameSource::Browser,
    }
}

async fn wait_for(
    manager: &DownloadManager,
    timeout: Duration,
    done: impl Fn(&downpour_core::DownloadView) -> bool,
) -> downpour_core::DownloadView {
    let deadline = Instant::now() + timeout;
    loop {
        let view = manager.snapshot().into_iter().next().expect("one download");
        if done(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "timed out; last status {:?}",
            view.status
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segmented_download_matches_source() {
    let body = sample_body(6 * 1024 * 1024 + 123);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("segmented");
    let manager = manager_in(&dir, 6);

    manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Queue,
        ))
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    let path = view.target.unwrap();
    assert_eq!(path.file_name().unwrap(), "sample.bin");
    assert_eq!(std::fs::read(&path).unwrap(), *body);
    assert!(
        server.range_requests.load(Ordering::Relaxed) > 1,
        "extra connections were opened"
    );
    assert!(view.segments.len() > 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_download_resumes_to_identical_file() {
    let body = sample_body(8 * 1024 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("resume");
    let manager = manager_in(&dir, 4);

    let id = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            Some("resumed.bin"),
            None,
            StartMode::Queue,
        ))
        .unwrap();
    wait_for(&manager, Duration::from_secs(30), |v| {
        v.received > 1024 * 1024
    })
    .await;
    manager.pause(id).unwrap();
    let paused = wait_for(&manager, Duration::from_secs(10), |v| {
        v.status == DownloadStatus::Paused
    })
    .await;
    assert!(
        paused.received < body.len() as u64,
        "paused before completion"
    );

    manager.resume(id).unwrap();
    let view = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    assert_eq!(std::fs::read(view.target.unwrap()).unwrap(), *body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_limited_server_completes_without_failing() {
    let body = sample_body(6 * 1024 * 1024);
    let server = serve(
        Arc::clone(&body),
        Behavior {
            ranges: RangeSupport::Ranges,
            max_concurrent: 2,
            refusal: Refusal::TooManyRequests,
            accepted_requests: usize::MAX,
        },
    )
    .await;
    let dir = temp_dir("limited");
    let manager = manager_in(&dir, 8);

    manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Queue,
        ))
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(30), |v| {
        matches!(
            v.status,
            DownloadStatus::Completed | DownloadStatus::Failed { .. }
        )
    })
    .await;

    assert_eq!(view.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(view.target.unwrap()).unwrap(), *body);
    // Only the initial burst of extra connections is refused; after that the
    // download stays within the limit it learned.
    let refused = server.refused_requests.load(Ordering::SeqCst);
    assert!(
        refused <= 7,
        "kept exceeding the server limit: {refused} refused requests"
    );
}

/// A server that answers connections beyond its limit with 404 must not
/// fail a download it is serving: a download once failed at 99% when a
/// connection replacing a finished one was refused this way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extra_connections_refused_as_not_found_still_complete() {
    let body = sample_body(6 * 1024 * 1024);
    let server = serve(
        Arc::clone(&body),
        Behavior {
            ranges: RangeSupport::Ranges,
            max_concurrent: 2,
            refusal: Refusal::NotFound,
            accepted_requests: usize::MAX,
        },
    )
    .await;
    let dir = temp_dir("refused-404");
    let manager = manager_in(&dir, 8);

    manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Queue,
        ))
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(60), |v| {
        matches!(
            v.status,
            DownloadStatus::Completed | DownloadStatus::Failed { .. }
        )
    })
    .await;

    assert_eq!(view.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(view.target.unwrap()).unwrap(), *body);
    assert!(
        server.refused_requests.load(Ordering::SeqCst) > 0,
        "the server never refused a connection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_without_ranges_streams_whole_file() {
    let body = sample_body(700 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::NoRanges).await;
    let dir = temp_dir("stream");
    let manager = manager_in(&dir, 8);

    manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Queue,
        ))
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    assert_eq!(view.resumable, Some(false));
    assert_eq!(std::fs::read(view.target.unwrap()).unwrap(), *body);
}

/// Answers every request to `path` with a small body and the given
/// `Content-Disposition`.
async fn serve_with_content_disposition(path: &str, header: &'static str) -> Url {
    serve_redirect(path, None, header).await
}

/// Like [`serve_with_content_disposition`], with requests to `path` sent on to
/// `redirect_to` when it is given.
async fn serve_redirect(
    path: &str,
    redirect_to: Option<&'static str>,
    header: &'static str,
) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}{path}", listener.local_addr().unwrap())).unwrap();
    let start = url.path().to_owned();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 1024];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let requested = request.split_whitespace().nth(1).unwrap_or("/");
            let reply = match redirect_to {
                Some(target) if requested == start => format!(
                    "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                _ => format!(
                    "HTTP/1.1 200 OK\r\nContent-Disposition: {header}\r\nContent-Length: 4\r\nConnection: close\r\n\r\nclip"
                ),
            };
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    url
}

/// The name a download of `url` is saved under, starting from `preset`.
async fn saved_name(case: &str, url: Url, preset: Option<FileName>) -> String {
    let dir = temp_dir(&format!("server-name-{case}"));
    let manager = manager_in(&dir, 1);

    manager
        .add(NewDownload {
            file_name: preset,
            ..NewDownload::from_user(url, None, None, StartMode::Queue)
        })
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    let path = view.target.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"clip", "{case}");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(view.file_name, name, "the list must show the saved name");
    name
}

const BLANK_NAME_HEADERS: [(&str, &str); 4] = [
    ("quoted-empty", r#"attachment; filename="""#),
    ("escaped-space", r#"attachment; filename="%20""#),
    ("only-dots", r#"attachment; filename="...""#),
    ("extended-empty", "attachment; filename*=UTF-8''"),
];

const REAL_NAME_HEADER: &str = r#"attachment; filename="Real%20Name.mp4""#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blank_server_name_keeps_the_name_in_the_url() {
    for (case, header) in BLANK_NAME_HEADERS {
        let url = serve_with_content_disposition("/files/clip.mp4", header).await;
        let name = saved_name(&format!("url-{case}"), url, None).await;
        assert_eq!(name, "clip.mp4", "{case}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blank_server_name_with_no_name_in_the_url_saves_as_unnamed() {
    for (case, header) in BLANK_NAME_HEADERS {
        let url = serve_with_content_disposition("/", header).await;
        let name = saved_name(&format!("bare-{case}"), url, None).await;
        assert_eq!(name, "unnamed", "{case}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_raw_non_ascii_server_name_is_used() {
    let header = "attachment; filename=\"na\u{ef}ve clip.mp4\"";
    let url = serve_with_content_disposition("/files/get", header).await;
    assert_eq!(saved_name("raw", url, None).await, "na\u{ef}ve clip.mp4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typed_name_of_nothing_lets_the_server_name_the_file() {
    let url = serve_with_content_disposition("/files/clip.mp4", REAL_NAME_HEADER).await;
    let dir = temp_dir("typed-nothing");
    let manager = manager_in(&dir, 1);

    manager
        .add(NewDownload::from_user(
            url,
            Some("..."),
            None,
            StartMode::Queue,
        ))
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    assert_eq!(view.file_name, "Real Name.mp4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_download_of_a_name_shows_the_numbered_name_it_saved_under() {
    let (_, blank) = BLANK_NAME_HEADERS[0];
    let url = serve_with_content_disposition("/files/clip.mp4", blank).await;
    let dir = temp_dir("collision");
    let manager = manager_in(&dir, 1);
    let completed = |v: &downpour_core::DownloadView| v.status == DownloadStatus::Completed;
    let timeout = Duration::from_secs(30);

    let request = || NewDownload::from_user(url.clone(), None, None, StartMode::Queue);
    let first = manager.add(request()).unwrap();
    wait_for_download(&manager, first, timeout, completed).await;
    let second = manager.add(request()).unwrap();
    let view = wait_for_download(&manager, second, timeout, completed).await;

    assert_eq!(view.file_name, "clip (1).mp4");
    assert_eq!(view.target.unwrap().file_name().unwrap(), "clip (1).mp4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_url_ending_in_download_is_a_real_name() {
    let (_, blank) = BLANK_NAME_HEADERS[0];
    let url = serve_with_content_disposition("/files/download", blank).await;
    assert_eq!(saved_name("literal", url, None).await, "download");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_server_name_beats_the_name_in_the_url() {
    let url = serve_with_content_disposition("/files/clip.mp4", REAL_NAME_HEADER).await;
    assert_eq!(saved_name("real", url, None).await, "Real Name.mp4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_someone_chose_beats_every_server_answer() {
    let (_, blank) = BLANK_NAME_HEADERS[0];
    let sources = [
        ("typed", FileNameSource::User),
        ("browser", FileNameSource::Browser),
    ];
    for (who, source) in sources {
        for (answer, header) in [("real", REAL_NAME_HEADER), ("blank", blank)] {
            let url = serve_with_content_disposition("/files/clip.mp4", header).await;
            let chosen = FileName::new("mine.mp4", source);
            let name = saved_name(&format!("{who}-{answer}"), url, Some(chosen)).await;
            assert_eq!(name, "mine.mp4", "{who} against a {answer} header");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_supplies_the_name_only_when_the_link_has_none() {
    let (_, blank) = BLANK_NAME_HEADERS[0];
    let bare = serve_redirect("/", Some("/files/clip.mp4"), blank).await;
    assert_eq!(saved_name("to-named", bare, None).await, "clip.mp4");
    let named = serve_redirect("/files/clip.mp4", Some("/opaque"), blank).await;
    assert_eq!(saved_name("from-named", named, None).await, "clip.mp4");
}

/// A server without ranges whose first response has a blank name and stalls
/// partway, and whose later responses name the file `Real Name.mp4`.
async fn serve_blank_name_then_real_name(body_len: usize) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/files/clip.mp4",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let is_first = requests.fetch_add(1, Ordering::SeqCst) == 0;
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer).await;
                let header = if is_first {
                    r#"attachment; filename="""#
                } else {
                    REAL_NAME_HEADER
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Disposition: {header}\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let sent = if is_first { body_len / 2 } else { body_len };
                let _ = socket.write_all(&vec![7u8; sent]).await;
                if is_first {
                    // Held open so the download is still running when paused.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            });
        }
    });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_keeps_the_name_of_the_file_already_on_disk() {
    let url = serve_blank_name_then_real_name(64 * 1024).await;
    let dir = temp_dir("restart-name");
    let manager = manager_in(&dir, 1);

    let id = manager
        .add(NewDownload::from_user(url, None, None, StartMode::Queue))
        .unwrap();
    wait_for(&manager, Duration::from_secs(30), |v| {
        v.target.is_some() && v.received > 0
    })
    .await;
    manager.pause(id).unwrap();
    wait_for(&manager, Duration::from_secs(10), |v| {
        v.status == DownloadStatus::Paused
    })
    .await;
    manager.resume(id).unwrap();
    let view = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    let saved = view.target.unwrap();
    assert_eq!(saved.file_name().unwrap(), "clip.mp4");
    assert_eq!(view.file_name, "clip.mp4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_half_finished_download_saved_with_escapes_finishes_with_a_clean_name() {
    let body = sample_body(8 * 1024 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("escaped-resume");
    let manager = manager_in(&dir, 4);
    let id = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            Some("placeholder.bin"),
            None,
            StartMode::Queue,
        ))
        .unwrap();
    wait_for(&manager, Duration::from_secs(30), |v| {
        v.received > 1024 * 1024
    })
    .await;
    manager.pause(id).unwrap();
    let paused = wait_for(&manager, Duration::from_secs(10), |v| {
        v.status == DownloadStatus::Paused
    })
    .await;
    assert!(
        paused.received < body.len() as u64,
        "paused before completion"
    );
    manager.shutdown(Duration::from_secs(5));
    drop(manager);

    let store = Store::open(dir.join("state")).unwrap();
    let mut records = store.load_downloads().unwrap();
    let old_target = dir.join("downloads").join("My%20Clip.bin");
    std::fs::rename(
        downpour_core::record::part_path(records[0].target.as_ref().unwrap()),
        downpour_core::record::part_path(&old_target),
    )
    .unwrap();
    records[0].file_name = escaped_browser_name("My%20Clip.bin");
    records[0].target = Some(old_target.clone());

    let restarted = manager_with_saved_list(&dir, 4, records);
    restarted.resume(id).unwrap();
    let finished = wait_for(&restarted, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    let saved = finished.target.unwrap();
    assert_eq!(saved.file_name().unwrap(), "My Clip.bin");
    assert_eq!(finished.file_name, "My Clip.bin");
    assert_eq!(std::fs::read(saved).unwrap(), *body);
    assert!(!downpour_core::record::part_path(&old_target).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarting_cleans_waiting_names_but_not_typed_or_finished_ones() {
    let server = serve(sample_body(1024), RangeSupport::Ranges).await;
    let dir = temp_dir("escaped-list");
    let manager = manager_in(&dir, 1);
    let waiting = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Hold,
        ))
        .unwrap();
    let typed = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            Some("100%20.bin"),
            None,
            StartMode::Hold,
        ))
        .unwrap();
    let finished = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Hold,
        ))
        .unwrap();
    manager.shutdown(Duration::from_secs(5));
    drop(manager);

    let store = Store::open(dir.join("state")).unwrap();
    let mut records = store.load_downloads().unwrap();
    for record in &mut records {
        if record.id == waiting {
            record.file_name = escaped_browser_name("Waiting%20Clip.bin");
        } else if record.id == finished {
            let done = dir.join("downloads").join("Done%20Clip.bin");
            record.file_name = escaped_browser_name("Done%20Clip.bin");
            record.target = Some(done);
            record.status = DownloadStatus::Completed;
        }
    }

    let restarted = manager_with_saved_list(&dir, 1, records);
    let name_of = |id| {
        restarted
            .snapshot()
            .into_iter()
            .find(|view| view.id == id)
            .unwrap()
            .file_name
    };
    assert_eq!(name_of(waiting), "Waiting Clip.bin");
    assert_eq!(name_of(typed), "100%20.bin");
    assert_eq!(name_of(finished), "Done%20Clip.bin");

    let on_disk = store.load_downloads().unwrap();
    let saved = on_disk.iter().find(|record| record.id == waiting).unwrap();
    assert_eq!(saved.file_name.value, "Waiting Clip.bin");
}

async fn wait_for_download(
    manager: &DownloadManager,
    id: downpour_core::DownloadId,
    timeout: Duration,
    done: impl Fn(&downpour_core::DownloadView) -> bool,
) -> downpour_core::DownloadView {
    let deadline = Instant::now() + timeout;
    loop {
        let view = manager
            .snapshot()
            .into_iter()
            .find(|view| view.id == id)
            .expect("download exists");
        if done(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "timed out; last status {:?}",
            view.status
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Answers every request with a tiny body that claims to be part of a
/// one-petabyte file, which can never fit on disk.
async fn serve_impossibly_large_file() -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/huge.iso",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer).await;
            let petabyte: u64 = 1 << 50;
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/{petabyte}\r\nConnection: close\r\n\r\nabcd"
            );
            let _ = socket.write_all(head.as_bytes()).await;
        }
    });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_too_large_for_disk_waits_for_space() {
    let url = serve_impossibly_large_file().await;
    let dir = temp_dir("no-space");
    let manager = manager_in(&dir, 2);

    let id = manager
        .add(NewDownload::from_user(url, None, None, StartMode::Queue))
        .unwrap();
    let view = wait_for_download(&manager, id, Duration::from_secs(10), |v| {
        matches!(
            v.status,
            DownloadStatus::WaitingForSpace { .. } | DownloadStatus::Failed { .. }
        )
    })
    .await;

    let DownloadStatus::WaitingForSpace {
        needed_bytes,
        directory,
    } = view.status
    else {
        panic!("expected to wait for disk space, got {:?}", view.status);
    };
    assert_eq!(needed_bytes, Some(1 << 50));
    assert_eq!(directory, dir.join("downloads"));
    // Nothing was allocated and the reserved name was given back.
    assert!(view.target.is_none());
    let leftovers: Vec<_> = std::fs::read_dir(&directory).unwrap().collect();
    assert!(leftovers.is_empty(), "files left behind: {leftovers:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopped_queue_holds_its_downloads_until_started() {
    let body = sample_body(512 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("queues");
    let manager = manager_in(&dir, 2);
    let later = manager.add_queue("Later").unwrap();
    manager.stop_queue(later).unwrap();

    let held = manager
        .add(NewDownload {
            queue: Some(later),
            ..NewDownload::from_user(server.url.clone(), Some("held.bin"), None, StartMode::Queue)
        })
        .unwrap();
    let main = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            Some("main.bin"),
            None,
            StartMode::Queue,
        ))
        .unwrap();

    wait_for_download(&manager, main, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    let still_held = wait_for_download(&manager, held, Duration::from_secs(1), |_| true).await;
    assert_eq!(still_held.status, DownloadStatus::Queued);

    manager.start_queue(later).unwrap();
    let finished = wait_for_download(&manager, held, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    assert_eq!(std::fs::read(finished.target.unwrap()).unwrap(), *body);
    // The queue survives a restart of the manager.
    let saved = Store::open(dir.join("state"))
        .unwrap()
        .load_queues()
        .unwrap()
        .unwrap();
    assert!(saved.all().iter().any(|queue| queue.name == "Later"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_runs_no_more_than_its_limit() {
    let body = sample_body(2 * 1024 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("queue-limit");
    let manager = manager_in(&dir, 2);
    manager
        .set_queue_limit(downpour_core::QueueId::MAIN, 1)
        .unwrap();

    for name in ["a.bin", "b.bin", "c.bin"] {
        manager
            .add(NewDownload::from_user(
                server.url.clone(),
                Some(name),
                None,
                StartMode::Queue,
            ))
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let views = manager.snapshot();
        let running = views.iter().filter(|v| v.status.is_active()).count();
        assert!(
            running <= 1,
            "{running} downloads ran at once in a queue limited to 1"
        );
        if views.iter().all(|v| v.status == DownloadStatus::Completed) {
            break;
        }
        assert!(Instant::now() < deadline, "queue did not finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_file_fails_with_status() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/missing",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
    });
    let dir = temp_dir("missing");
    let manager = manager_in(&dir, 2);

    manager
        .add(NewDownload::from_user(url, None, None, StartMode::Queue))
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(10), |v| {
        matches!(v.status, DownloadStatus::Failed { .. })
    })
    .await;
    let DownloadStatus::Failed { reason } = view.status else {
        unreachable!()
    };
    assert!(reason.contains("404"), "{reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_later_waits_until_its_queue_is_started() {
    let body = sample_body(256 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("download-later");
    let manager = manager_in(&dir, 2);
    let later = manager.add_queue("Later").unwrap();

    let id = manager
        .add(NewDownload {
            queue: Some(later),
            ..NewDownload::from_user(server.url.clone(), Some("later.bin"), None, StartMode::Hold)
        })
        .unwrap();
    // A running queue does not start a download added "later".
    let held = wait_for_download(&manager, id, Duration::from_secs(1), |_| true).await;
    assert_eq!(held.status, DownloadStatus::Paused);

    manager.start_queue(later).unwrap();
    let finished = wait_for_download(&manager, id, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    assert_eq!(std::fs::read(finished.target.unwrap()).unwrap(), *body);
}

/// The order the user sets is the order a queue starts its downloads in,
/// and the order the list shows them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moving_a_download_to_the_top_starts_it_first() {
    let body = sample_body(2 * 1024 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("reorder");
    let manager = manager_in(&dir, 2);
    manager
        .set_queue_limit(downpour_core::QueueId::MAIN, 1)
        .unwrap();
    manager.stop_queue(downpour_core::QueueId::MAIN).unwrap();

    let mut ids = Vec::new();
    for name in ["a.bin", "b.bin", "c.bin"] {
        ids.push(
            manager
                .add(NewDownload::from_user(
                    server.url.clone(),
                    Some(name),
                    None,
                    StartMode::Queue,
                ))
                .unwrap(),
        );
    }
    manager
        .move_in_queue(&[ids[2]], downpour_core::QueuePlace::Before(ids[0]))
        .unwrap();
    let names: Vec<String> = manager
        .snapshot()
        .into_iter()
        .map(|v| v.file_name)
        .collect();
    assert_eq!(names, ["c.bin", "a.bin", "b.bin"]);

    manager.start_queue(downpour_core::QueueId::MAIN).unwrap();
    let first = wait_for(&manager, Duration::from_secs(20), |v| {
        v.status.is_active() || v.status == DownloadStatus::Completed
    })
    .await;
    assert_eq!(
        first.file_name, "c.bin",
        "the download moved to the top did not start first"
    );
}

/// A link that stops accepting new requests partway (while open connections
/// keep streaming) must still finish: the connections that are running own
/// their ranges until a split's new connection is accepted. A download
/// once stalled at 99% when every late split was refused with 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_that_stops_accepting_new_requests_still_finishes() {
    let body = sample_body(6 * 1024 * 1024);
    let server = serve(
        Arc::clone(&body),
        Behavior {
            ranges: RangeSupport::Ranges,
            max_concurrent: usize::MAX,
            refusal: Refusal::NotFound,
            // The probe and one more connection, then nothing new.
            accepted_requests: 2,
        },
    )
    .await;
    let dir = temp_dir("link-stops-accepting");
    let manager = manager_in(&dir, 8);

    manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Queue,
        ))
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(60), |v| {
        matches!(
            v.status,
            DownloadStatus::Completed | DownloadStatus::Failed { .. }
        )
    })
    .await;

    assert_eq!(view.status, DownloadStatus::Completed);
    assert_eq!(std::fs::read(view.target.unwrap()).unwrap(), *body);
    assert!(
        server.refused_requests.load(Ordering::SeqCst) > 0,
        "the link never refused a request"
    );
}

/// "Start now" runs a waiting download although its queue is stopped; the
/// others keep waiting, and a download that is not waiting is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_now_runs_a_download_its_stopped_queue_holds_back() {
    let body = sample_body(512 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("start-now");
    let manager = manager_in(&dir, 2);
    manager.stop_queue(downpour_core::QueueId::MAIN).unwrap();
    let add = |name: &str| {
        manager
            .add(NewDownload::from_user(
                server.url.clone(),
                Some(name),
                None,
                StartMode::Queue,
            ))
            .unwrap()
    };
    let forced = add("forced.bin");
    let waiting = add("waiting.bin");

    manager.start_now(forced).unwrap();
    let done = wait_for_download(&manager, forced, Duration::from_secs(20), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    assert_eq!(std::fs::read(done.target.unwrap()).unwrap(), *body);
    let other = manager
        .snapshot()
        .into_iter()
        .find(|v| v.id == waiting)
        .unwrap();
    assert_eq!(other.status, DownloadStatus::Queued);
    assert!(matches!(
        manager.start_now(forced),
        Err(downpour_core::ManagerError::NotWaiting(_))
    ));
}

/// A download set to "no limit" is not held to the speed limit from
/// Settings: 2 MiB at 16 KiB/s would take two minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_without_a_limit_ignores_the_global_one() {
    let body = sample_body(2 * 1024 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("no-limit");
    let manager = manager_in(&dir, 2);
    manager.update_settings(EngineSettings {
        speed_limit_kib_per_second: std::num::NonZeroU32::new(16),
        ..manager.settings()
    });
    manager.stop_queue(downpour_core::QueueId::MAIN).unwrap();
    let id = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            Some("free.bin"),
            None,
            StartMode::Queue,
        ))
        .unwrap();
    manager
        .set_speed_limit(id, downpour_core::limiter::DownloadLimit::Unlimited)
        .unwrap();
    manager.start_queue(downpour_core::QueueId::MAIN).unwrap();
    let done = wait_for_download(&manager, id, Duration::from_secs(20), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    assert_eq!(
        done.speed_limit,
        downpour_core::limiter::DownloadLimit::Unlimited
    );
    assert_eq!(std::fs::read(done.target.unwrap()).unwrap(), *body);
}

/// Lifting the limit of a running download takes effect at once: 2 MiB at
/// the 16 KiB/s global limit would take two minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifting_the_limit_of_a_running_download_speeds_it_up() {
    let body = sample_body(2 * 1024 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("lift-limit");
    let manager = manager_in(&dir, 2);
    manager.update_settings(EngineSettings {
        speed_limit_kib_per_second: std::num::NonZeroU32::new(16),
        ..manager.settings()
    });
    let id = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            Some("lifted.bin"),
            None,
            StartMode::Queue,
        ))
        .unwrap();
    wait_for_download(&manager, id, Duration::from_secs(20), |v| {
        v.status == DownloadStatus::Downloading
    })
    .await;
    // Long enough for the connections to be deep in the global limit's debt.
    tokio::time::sleep(Duration::from_secs(1)).await;
    manager
        .set_speed_limit(id, downpour_core::limiter::DownloadLimit::Unlimited)
        .unwrap();
    let done = wait_for_download(&manager, id, Duration::from_secs(20), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    assert_eq!(std::fs::read(done.target.unwrap()).unwrap(), *body);
}

/// "Start now" also runs a download its full queue holds back, next to the
/// one already running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_now_runs_a_download_past_a_full_queue() {
    let body = sample_body(1024 * 1024);
    let server = serve(Arc::clone(&body), RangeSupport::Ranges).await;
    let dir = temp_dir("start-now-full");
    let manager = manager_in(&dir, 2);
    manager
        .set_queue_limit(downpour_core::QueueId::MAIN, 1)
        .unwrap();
    // The first download crawls at the global limit and keeps the one place.
    manager.update_settings(EngineSettings {
        speed_limit_kib_per_second: std::num::NonZeroU32::new(16),
        ..manager.settings()
    });
    let add = |name: &str| {
        manager
            .add(NewDownload::from_user(
                server.url.clone(),
                Some(name),
                None,
                StartMode::Queue,
            ))
            .unwrap()
    };
    let slow = add("slow.bin");
    let forced = add("forced.bin");
    wait_for_download(&manager, slow, Duration::from_secs(20), |v| {
        v.status.is_active()
    })
    .await;
    let waiting = manager
        .snapshot()
        .into_iter()
        .find(|v| v.id == forced)
        .unwrap();
    assert!(
        waiting.can_start_now,
        "the second download should be waiting"
    );

    manager
        .set_speed_limit(forced, downpour_core::limiter::DownloadLimit::Unlimited)
        .unwrap();
    manager.start_now(forced).unwrap();
    let done = wait_for_download(&manager, forced, Duration::from_secs(20), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    assert_eq!(std::fs::read(done.target.unwrap()).unwrap(), *body);
    let first = manager
        .snapshot()
        .into_iter()
        .find(|v| v.id == slow)
        .unwrap();
    assert!(first.status.is_active(), "the slow download keeps running");
}

/// A finished download of a small file, and the manager that holds it.
async fn finished_download(case: &str) -> (DownloadManager, downpour_core::DownloadView, PathBuf) {
    let body = sample_body(64 * 1024);
    let server = serve(body, RangeSupport::Ranges).await;
    let dir = temp_dir(case);
    let manager = manager_in(&dir, 1);
    manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Queue,
        ))
        .unwrap();
    let view = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;
    (manager, view, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_file_is_renamed_in_its_folder() {
    let (manager, view, _dir) = finished_download("rename").await;
    let before = view.target.clone().unwrap();

    let Renamed::Now(renamed) = manager.rename_file(view.id, "Holiday clip.bin").unwrap() else {
        panic!("a finished file is renamed at once");
    };

    assert_eq!(renamed, before.with_file_name("Holiday clip.bin"));
    assert!(renamed.is_file());
    assert!(!before.exists());
    let shown = manager.snapshot().into_iter().next().unwrap();
    assert_eq!(shown.file_name, "Holiday clip.bin");
    assert_eq!(shown.target.as_deref(), Some(renamed.as_path()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renaming_a_moved_file_says_where_it_was_expected() {
    let (manager, view, _dir) = finished_download("rename-moved").await;
    let target = view.target.clone().unwrap();
    std::fs::remove_file(&target).unwrap();

    let error = manager.rename_file(view.id, "new.bin").unwrap_err();

    assert!(matches!(&error, RenameError::FileMissing(path) if *path == target));
    assert!(error.to_string().contains("moved or deleted"), "{error}");
    assert_eq!(
        manager.snapshot()[0].target.as_deref(),
        Some(target.as_path())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renaming_never_replaces_another_file() {
    let (manager, view, _dir) = finished_download("rename-taken").await;
    let target = view.target.clone().unwrap();
    let other = target.with_file_name("other.bin");
    std::fs::write(&other, b"keep me").unwrap();

    let error = manager.rename_file(view.id, "other.bin").unwrap_err();

    assert!(matches!(&error, RenameError::NameTaken(path) if *path == other));
    assert_eq!(std::fs::read(&other).unwrap(), b"keep me");
    assert!(target.is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rename_takes_only_a_name() {
    let (manager, view, _dir) = finished_download("rename-name").await;

    assert!(matches!(
        manager.rename_file(view.id, "sub/clip.bin"),
        Err(RenameError::NameHasFolder)
    ));
    assert!(matches!(
        manager.rename_file(view.id, "   "),
        Err(RenameError::UnusableName(_))
    ));
    assert!(view.target.unwrap().is_file());
}

/// A download held to 1 KiB/s once its transfer reserved a name, so a test
/// can act while it runs. Lift the limit to let it finish.
async fn running_download(case: &str) -> RunningDownload {
    let server = serve(sample_body(64 * 1024), RangeSupport::Ranges).await;
    let dir = temp_dir(case);
    let manager = manager_in(&dir, 1);
    let id = start_slowly(&manager, &server).await;
    let reserved = manager.snapshot()[0].target.clone().unwrap();
    RunningDownload {
        manager,
        id,
        reserved,
        dir,
        _server: server,
    }
}

struct RunningDownload {
    manager: DownloadManager,
    id: downpour_core::DownloadId,
    /// The name its transfer writes under.
    reserved: PathBuf,
    dir: PathBuf,
    _server: TestServer,
}

/// Adds a download of `server`'s file at 1 KiB/s and waits until its
/// transfer reserved a name.
async fn start_slowly(manager: &DownloadManager, server: &TestServer) -> downpour_core::DownloadId {
    let id = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Hold,
        ))
        .unwrap();
    let slow = downpour_core::limiter::DownloadLimit::Limited {
        kib_per_second: std::num::NonZeroU32::MIN,
    };
    manager.set_speed_limit(id, slow).unwrap();
    manager.resume(id).unwrap();
    wait_for_download(manager, id, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Downloading && v.target.is_some()
    })
    .await;
    id
}

async fn finish(
    manager: &DownloadManager,
    id: downpour_core::DownloadId,
) -> downpour_core::DownloadView {
    manager
        .set_speed_limit(id, downpour_core::limiter::DownloadLimit::Unlimited)
        .unwrap();
    wait_for(manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_download_takes_its_new_name_when_finished() {
    let RunningDownload {
        manager,
        id,
        reserved,
        ..
    } = running_download("rename-running").await;

    assert_eq!(
        manager.rename_file(id, "Holiday clip.bin").unwrap(),
        Renamed::WhenFinished("Holiday clip.bin".to_owned())
    );
    let running = manager.snapshot().into_iter().next().unwrap();
    assert_eq!(running.file_name, "Holiday clip.bin");
    assert_eq!(running.target.as_deref(), Some(reserved.as_path()));

    let done = finish(&manager, id).await;
    let renamed = reserved.with_file_name("Holiday clip.bin");
    assert_eq!(done.target.as_deref(), Some(renamed.as_path()));
    assert_eq!(done.file_name, "Holiday clip.bin");
    assert_eq!(std::fs::metadata(&renamed).unwrap().len(), 64 * 1024);
    assert!(!reserved.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_not_started_yet_takes_its_new_name_when_finished() {
    let server = serve(sample_body(1024), RangeSupport::Ranges).await;
    let manager = manager_in(&temp_dir("rename-held"), 1);
    let id = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Hold,
        ))
        .unwrap();

    assert_eq!(
        manager.rename_file(id, "clip.bin").unwrap(),
        Renamed::WhenFinished("clip.bin".to_owned())
    );
    manager.resume(id).unwrap();
    let done = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    let target = done.target.unwrap();
    assert_eq!(target.file_name().unwrap(), "clip.bin");
    assert!(target.is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_taken_before_the_download_finishes_is_refused() {
    let RunningDownload {
        manager,
        id,
        reserved,
        ..
    } = running_download("rename-running-taken").await;
    let other = reserved.with_file_name("other.bin");
    std::fs::write(&other, b"keep me").unwrap();

    let error = manager.rename_file(id, "other.bin").unwrap_err();

    assert!(matches!(&error, RenameError::NameTaken(path) if *path == other));
    let done = finish(&manager, id).await;
    assert_eq!(done.target.as_deref(), Some(reserved.as_path()));
    assert_eq!(std::fs::read(&other).unwrap(), b"keep me");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_taken_while_downloading_gets_a_number() {
    let RunningDownload {
        manager,
        id,
        reserved,
        ..
    } = running_download("rename-taken-later").await;
    manager.rename_file(id, "other.bin").unwrap();
    let other = reserved.with_file_name("other.bin");
    std::fs::write(&other, b"keep me").unwrap();

    let done = finish(&manager, id).await;

    let numbered = reserved.with_file_name("other (1).bin");
    assert_eq!(done.target.as_deref(), Some(numbered.as_path()));
    assert_eq!(done.file_name, "other (1).bin");
    assert!(numbered.is_file());
    assert_eq!(std::fs::read(&other).unwrap(), b"keep me");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_chosen_while_paused_survives_a_restart() {
    let RunningDownload {
        manager,
        id,
        reserved,
        dir,
        _server,
    } = running_download("rename-restart").await;
    manager.pause(id).unwrap();
    wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Paused
    })
    .await;
    manager.rename_file(id, "kept.bin").unwrap();
    manager.shutdown(Duration::from_secs(5));
    drop(manager);

    let saved = Store::open(dir.join("state"))
        .unwrap()
        .load_downloads()
        .unwrap();
    let reopened = manager_with_saved_list(&dir, 1, saved);
    assert_eq!(reopened.snapshot()[0].file_name, "kept.bin");
    reopened
        .set_speed_limit(id, downpour_core::limiter::DownloadLimit::Unlimited)
        .unwrap();
    reopened.resume(id).unwrap();
    let done = wait_for(&reopened, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    let kept = reserved.with_file_name("kept.bin");
    assert_eq!(done.target.as_deref(), Some(kept.as_path()));
    assert_eq!(std::fs::metadata(&kept).unwrap().len(), 64 * 1024);
}

/// A manager that files downloads in category folders, and the folder a
/// file named `name` is filed in.
fn filing_manager(
    dir: &Path,
    records: Vec<downpour_core::record::DownloadRecord>,
    name: &str,
) -> (DownloadManager, PathBuf) {
    let settings = EngineSettings {
        download_dir: dir.join("downloads"),
        organize_by_category: true,
        connections_per_download: 1,
        ..Default::default()
    };
    let folder = settings
        .categories
        .save_folder_for(name, &dir.join("downloads"));
    let store = Store::open(dir.join("state")).unwrap();
    let manager =
        DownloadManager::start(store, records, settings, tokio::runtime::Handle::current())
            .unwrap();
    (manager, folder)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_not_started_yet_is_saved_in_its_new_names_category() {
    let server = serve(sample_body(1024), RangeSupport::Ranges).await;
    let dir = temp_dir("rename-category");
    let (manager, expected_folder) = filing_manager(&dir, Vec::new(), "clip.zip");
    let id = manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Hold,
        ))
        .unwrap();

    manager.rename_file(id, "clip.zip").unwrap();
    manager.resume(id).unwrap();
    let done = wait_for(&manager, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    assert_eq!(
        done.target.as_deref(),
        Some(expected_folder.join("clip.zip").as_path())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_chosen_before_the_file_was_reserved_files_it_by_that_name() {
    let server = serve(sample_body(1024), RangeSupport::Ranges).await;
    let dir = temp_dir("rename-before-reserved");
    let (manager, _) = filing_manager(&dir, Vec::new(), "clip.zip");
    manager
        .add(NewDownload::from_user(
            server.url.clone(),
            None,
            None,
            StartMode::Hold,
        ))
        .unwrap();
    manager.shutdown(Duration::from_secs(5));
    drop(manager);
    // What renaming while the first run was still connecting leaves: the
    // name waits for the finish, and no file was reserved.
    let mut saved = Store::open(dir.join("state"))
        .unwrap()
        .load_downloads()
        .unwrap();
    saved[0].name_when_finished = FileName::from_typed("clip.zip");
    assert_eq!(saved[0].target, None);

    let (reopened, expected_folder) = filing_manager(&dir, saved, "clip.zip");
    reopened.resume(saved_id(&reopened)).unwrap();
    let done = wait_for(&reopened, Duration::from_secs(30), |v| {
        v.status == DownloadStatus::Completed
    })
    .await;

    assert_eq!(
        done.target.as_deref(),
        Some(expected_folder.join("clip.zip").as_path())
    );
}

fn saved_id(manager: &DownloadManager) -> downpour_core::DownloadId {
    manager.snapshot()[0].id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_another_download_reserved_is_refused() {
    let server = serve(sample_body(64 * 1024), RangeSupport::Ranges).await;
    let manager = manager_in(&temp_dir("rename-reserved"), 1);
    let first = start_slowly(&manager, &server).await;
    let second = start_slowly(&manager, &server).await;
    let reserved_by_second = manager
        .snapshot()
        .into_iter()
        .find(|view| view.id == second)
        .and_then(|view| view.target)
        .unwrap();
    let taken = reserved_by_second.file_name().unwrap().to_string_lossy();

    let error = manager.rename_file(first, &taken).unwrap_err();

    assert!(matches!(&error, RenameError::NameTaken(path) if *path == reserved_by_second));
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_only_the_case_renames_the_file() {
    let (manager, view, _dir) = finished_download("rename-case").await;
    let target = view.target.clone().unwrap();
    let upper = target.file_name().unwrap().to_string_lossy().to_uppercase();

    let Renamed::Now(renamed) = manager.rename_file(view.id, &upper).unwrap() else {
        panic!("a finished file is renamed at once");
    };

    let listed: Vec<String> = std::fs::read_dir(renamed.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(listed, vec![upper]);
}

/// Queued downloads that never start: the queue is stopped and the address
/// is never asked for.
fn held_downloads(manager: &DownloadManager, names: &[&str]) -> Vec<downpour_core::DownloadId> {
    manager.stop_queue(downpour_core::QueueId::MAIN).unwrap();
    names
        .iter()
        .map(|name| {
            let url = Url::parse(&format!("http://127.0.0.1:9/{name}")).unwrap();
            manager
                .add(NewDownload::from_user(
                    url,
                    Some(name),
                    None,
                    StartMode::Queue,
                ))
                .unwrap()
        })
        .collect()
}

fn names_in_order(manager: &DownloadManager) -> Vec<String> {
    manager
        .snapshot()
        .into_iter()
        .map(|view| view.file_name)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_moves_together_in_the_start_order() {
    let dir = temp_dir("group-reorder");
    let manager = manager_in(&dir, 1);
    let ids = held_downloads(&manager, &["a.bin", "b.bin", "c.bin"]);

    // Picked out of order, the group keeps the order it had.
    manager
        .move_in_queue(&[ids[2], ids[0]], downpour_core::QueuePlace::Before(ids[1]))
        .unwrap();

    assert_eq!(names_in_order(&manager), ["a.bin", "c.bin", "b.bin"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_to_a_queue_moves_all_or_none() {
    let dir = temp_dir("group-queue");
    let manager = manager_in(&dir, 1);
    let ids = held_downloads(&manager, &["a.bin", "b.bin"]);
    let later = manager.add_queue("Later").unwrap();

    let unknown = downpour_core::DownloadId(u64::MAX);
    assert!(matches!(
        manager.move_to_queue(&[ids[0], unknown], later),
        Err(downpour_core::ManagerError::UnknownDownload(id)) if id == unknown
    ));
    let queues: Vec<_> = manager.snapshot().iter().map(|view| view.queue).collect();
    assert_eq!(
        queues,
        [downpour_core::QueueId::MAIN, downpour_core::QueueId::MAIN]
    );

    manager.move_to_queue(&ids, later).unwrap();
    let queues: Vec<_> = manager.snapshot().iter().map(|view| view.queue).collect();
    assert_eq!(queues, [later, later]);
}

/// A manager that shut down writes nothing more: another one may own the
/// list by then, as when the app restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manager_saves_nothing_after_shutdown() {
    let dir = temp_dir("closed");
    let manager = manager_in(&dir, 1);
    held_downloads(&manager, &["a.bin"]);
    manager.shutdown(Duration::from_secs(5));

    // Someone else's list now, then a change that would have been saved.
    let store = Store::open(dir.join("state")).unwrap();
    store.save_downloads(&[]).unwrap();
    held_downloads(&manager, &["b.bin"]);
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(store.load_downloads().unwrap().is_empty());
}
