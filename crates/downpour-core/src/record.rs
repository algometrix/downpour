use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::category::Categories;
use crate::http::{RequestContext, Validators};
use crate::limiter::DownloadLimit;
use crate::naming::{ChosenStem, FileName, FileNameSource, NameParts};
use crate::queue::QueueId;
use crate::segment::Segment;
use crate::status::DownloadStatus;
use crate::stream::{StreamFormat, TrackRole, VariantInfo, VideoQuality};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DownloadId(pub u64);

impl std::fmt::Display for DownloadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Where a new download should be saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Destination {
    /// The user picked this folder.
    Folder { path: PathBuf },
    /// The folder of the file's category (by default `base/<category>/`),
    /// decided once the final file name is known.
    ByCategory { base: PathBuf },
}

impl Destination {
    pub fn directory_for(&self, file_name: &str, categories: &Categories) -> PathBuf {
        match self {
            Destination::Folder { path } => path.clone(),
            Destination::ByCategory { base } => categories.save_folder_for(file_name, base),
        }
    }
}

/// What is known about the bytes on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransferProgress {
    NotStarted,
    /// Range-capable download; resumable from these segments.
    Segmented {
        size: u64,
        segments: Vec<Segment>,
    },
    /// Sequential download; restarts from zero when interrupted.
    Streamed {
        size: Option<u64>,
        received: u64,
    },
    /// Video stream; each track resumes after its last written piece.
    Media {
        tracks: Vec<TrackProgress>,
    },
}

/// How far one track of a video stream has been written to disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackProgress {
    pub role: TrackRole,
    pub pieces_total: usize,
    /// Pieces written in order from the start; the file holds exactly these.
    pub pieces_done: usize,
    pub bytes: u64,
}

impl TrackProgress {
    pub fn is_complete(&self) -> bool {
        self.pieces_done >= self.pieces_total
    }

    /// The track's size extrapolated from the pieces written so far.
    fn estimated_size(&self) -> Option<u64> {
        if self.is_complete() {
            return Some(self.bytes);
        }
        (self.pieces_done > 0)
            .then(|| self.bytes / self.pieces_done as u64 * self.pieces_total as u64)
    }
}

impl TransferProgress {
    pub fn received_bytes(&self) -> u64 {
        match self {
            TransferProgress::NotStarted => 0,
            TransferProgress::Segmented { segments, .. } => {
                segments.iter().map(|s| s.written).sum()
            }
            TransferProgress::Streamed { received, .. } => *received,
            TransferProgress::Media { tracks } => tracks.iter().map(|track| track.bytes).sum(),
        }
    }

    /// The exact size, or for video streams an estimate from the pieces so far.
    pub fn total_size(&self) -> Option<u64> {
        match self {
            TransferProgress::NotStarted => None,
            TransferProgress::Segmented { size, .. } => Some(*size),
            TransferProgress::Streamed { size, .. } => *size,
            TransferProgress::Media { tracks } => tracks
                .iter()
                .map(TrackProgress::estimated_size)
                .sum::<Option<u64>>(),
        }
    }

    pub fn size_is_estimate(&self) -> bool {
        matches!(self, TransferProgress::Media { tracks } if tracks.iter().any(|t| !t.is_complete()))
    }

    /// Share of the work done, from 0 to 1, when it can be told.
    pub fn fraction(&self) -> Option<f32> {
        if let TransferProgress::Media { tracks } = self {
            let total: usize = tracks.iter().map(|track| track.pieces_total).sum();
            let done: usize = tracks
                .iter()
                .map(|track| track.pieces_done.min(track.pieces_total))
                .sum();
            return (total > 0).then(|| done as f32 / total as f32);
        }
        let size = self.total_size().filter(|size| *size > 0)?;
        Some((self.received_bytes() as f64 / size as f64).clamp(0.0, 1.0) as f32)
    }

    pub fn is_resumable(&self) -> Option<bool> {
        match self {
            TransferProgress::NotStarted => None,
            TransferProgress::Segmented { .. } | TransferProgress::Media { .. } => Some(true),
            TransferProgress::Streamed { .. } => Some(false),
        }
    }
}

/// What the URL points to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownloadSource {
    /// A single file, fetched with byte ranges when the server allows it.
    #[default]
    File,
    /// An HLS or DASH manifest whose pieces are joined into one video.
    Stream {
        format: StreamFormat,
        quality: VideoQuality,
        /// The quality actually chosen, once the manifest was read.
        #[serde(default)]
        variant: Option<VariantInfo>,
    },
}

/// Everything persisted about one download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRecord {
    pub id: DownloadId,
    pub url: Url,
    /// The page the download was started from, to find a fresh address
    /// when this one stops working.
    #[serde(default)]
    pub page_url: Option<Url>,
    #[serde(default)]
    pub queue: QueueId,
    #[serde(default)]
    pub source: DownloadSource,
    pub context: RequestContext,
    pub file_name: FileName,
    pub destination: Destination,
    /// Final path, fixed once the first transfer reserved it on disk.
    pub target: Option<PathBuf>,
    pub progress: TransferProgress,
    pub validators: Validators,
    pub status: DownloadStatus,
    pub connections: u8,
    pub added_at_unix: u64,
    pub finished_at_unix: Option<u64>,
    /// Place in the start order, lowest first; the user can rearrange it.
    /// Lists saved before it existed read 0 here and keep their old order
    /// (by id) when the engine renumbers them at start.
    #[serde(default)]
    pub position: u64,
    #[serde(default)]
    pub speed_limit: DownloadLimit,
    /// A name the user chose before the download finished. The transfer
    /// keeps writing under `target`; once complete, the file takes this stem
    /// with the extension it was saved with.
    #[serde(
        default,
        alias = "name_when_finished",
        deserialize_with = "stem_or_legacy_name"
    )]
    pub stem_when_finished: Option<ChosenStem>,
}

/// Lists saved before renames kept the extension hold a whole name, as
/// `{"value": "Holiday.mp4", "source": "user"}`. Its stem is what the user
/// chose; the extension comes from the file, as for any rename now.
fn stem_or_legacy_name<'de, D>(deserializer: D) -> Result<Option<ChosenStem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Saved {
        Stem(ChosenStem),
        LegacyName(FileName),
        /// A pending rename lost is better than a list that will not load.
        Unreadable(serde::de::IgnoredAny),
    }
    Ok(match Option::<Saved>::deserialize(deserializer)? {
        None => None,
        Some(Saved::Stem(stem)) => Some(stem),
        Some(Saved::LegacyName(name)) => ChosenStem::from_typed(NameParts::of(&name.value).stem),
        Some(Saved::Unreadable(_)) => {
            tracing::warn!("a saved rename could not be read; the download keeps its name");
            None
        }
    })
}

impl DownloadRecord {
    /// Where the file is, once the download finished.
    pub fn finished_file(&self) -> Option<&Path> {
        finished_file(&self.status, self.target.as_deref())
    }

    /// What a rename may change, and the extension the file keeps. The stem
    /// is the one the user chose, if any, kept whole: a typed `.mkv` in it is
    /// not taken for an extension.
    pub fn rename_parts(&self) -> RenameParts {
        let current = NameParts::of(&self.file_name.value);
        let stem = match &self.stem_when_finished {
            Some(stem) => stem.as_str().to_owned(),
            None => current.stem.to_owned(),
        };
        // A reserved file has its extension. Before that, only a name the
        // user, browser, or server gave is kept as it is (see
        // `FileName::after_response`); a stream's container always decides.
        let name_is_final = matches!(
            self.file_name.source,
            FileNameSource::User | FileNameSource::Browser | FileNameSource::Server
        ) && !matches!(self.source, DownloadSource::Stream { .. });
        let extension = if self.target.is_some() || name_is_final {
            RenameExtension::Kept(current.extension.map(str::to_owned))
        } else {
            RenameExtension::DecidedOnSave
        };
        RenameParts { stem, extension }
    }

    /// The name to show: the one the user chose, even before the file has it.
    /// Until the file is reserved its extension is the best guess so far.
    pub fn shown_name(&self) -> FileName {
        match &self.stem_when_finished {
            Some(stem) => stem.named_like(&self.file_name.value),
            None => self.file_name.clone(),
        }
    }

    pub fn directory(&self, categories: &Categories) -> PathBuf {
        match &self.target {
            Some(target) => target.parent().map(Path::to_path_buf).unwrap_or_default(),
            None => self
                .destination
                .directory_for(&self.file_name.value, categories),
        }
    }
}

/// What a rename changes: the stem. The extension stays the file's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameParts {
    pub stem: String,
    pub extension: RenameExtension,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameExtension {
    /// The file keeps this extension (`mp4`), or has none.
    Kept(Option<String>),
    /// No file is reserved and the name so far is a guess: the server's name
    /// or a stream's container decides the extension when the file is saved.
    DecidedOnSave,
}

/// The in-progress file that becomes `target` when the download completes.
pub fn part_path(target: &Path) -> PathBuf {
    let mut path = target.as_os_str().to_owned();
    path.push(".dppart");
    PathBuf::from(path)
}

/// Most tracks a stream download writes: video and audio.
pub const MAX_STREAM_TRACKS: usize = 2;

/// Where track `index` of a video stream is written before the merge.
pub fn track_path(target: &Path, index: usize) -> PathBuf {
    let mut path = target.as_os_str().to_owned();
    path.push(format!(".track{index}.dppart"));
    PathBuf::from(path)
}

/// The part file and any stream track files a download saving to `target`
/// writes before it finishes.
pub fn unfinished_files(target: &Path) -> Vec<PathBuf> {
    std::iter::once(part_path(target))
        .chain((0..MAX_STREAM_TRACKS).map(|index| track_path(target, index)))
        .collect()
}

/// A download's file counts as there to open or rename only once the
/// download finished; before that `target` is a reserved name.
pub(crate) fn finished_file<'a>(
    status: &DownloadStatus,
    target: Option<&'a Path>,
) -> Option<&'a Path> {
    match status {
        DownloadStatus::Completed => target,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_finished_download_has_its_file_there() {
        let file = Path::new("downloads/clip.mp4");
        assert_eq!(
            finished_file(&DownloadStatus::Completed, Some(file)),
            Some(file)
        );
        assert_eq!(finished_file(&DownloadStatus::Completed, None), None);
        // A restarted download keeps its reserved name but has no file yet.
        assert_eq!(finished_file(&DownloadStatus::Queued, Some(file)), None);
        assert_eq!(finished_file(&DownloadStatus::Paused, Some(file)), None);
    }
}
