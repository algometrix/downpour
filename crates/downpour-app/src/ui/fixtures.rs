//! Downloads to build test cases from.

use downpour_core::{DownloadId, DownloadStatus, DownloadView};

/// A queued download in the main queue named `file-<id>.zip`.
pub fn view(id: u64) -> DownloadView {
    DownloadView {
        id: DownloadId(id),
        file_name: format!("file-{id}.zip"),
        url: url::Url::parse("https://example.com/file.zip").unwrap(),
        page_url: None,
        referrer: None,
        queue: downpour_core::QueueId::MAIN,
        directory: std::path::PathBuf::from("downloads"),
        target: None,
        category: downpour_core::CategoryId::OTHER,
        status: DownloadStatus::Queued,
        size: None,
        received: 0,
        bytes_per_second: 0,
        connections: 0,
        segments: Vec::new(),
        resumable: None,
        size_is_estimate: false,
        progress_fraction: None,
        stream: None,
        added_at_unix: 0,
        finished_at_unix: None,
        stopping: false,
        speed_limit: downpour_core::limiter::DownloadLimit::FollowSettings,
        can_start_now: false,
    }
}
