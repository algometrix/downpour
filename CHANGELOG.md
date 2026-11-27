# Changelog

All notable changes to Downpour are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [1.1.0] - 2026-11-27

### Added

- Pick several downloads at once: click, Shift+click for a run, Ctrl+click to
  add or drop one, and Ctrl+Shift+click to add a run.
- Usual Windows list keys: Ctrl+A, Shift+Up/Down, Shift+Home/End, Home/End,
  and Ctrl+C to copy the picked addresses.
- Drag one or more downloads onto a queue in the sidebar to move them there,
  or drop between rows in a queue view to reorder them.
- The right-click menu and Remove work on a whole group of picked downloads.
- Captured videos are named after the page title, while the tab still shows
  the site that started them.
- A "Capture videos from this computer" switch in the Firefox add-on, off by
  default, so videos from `localhost` and `127.0.0.1` are left alone.
- A one-time notification the first time closing the window sends it to the
  system tray.

### Changed

- Minimizing keeps the window on the taskbar, like other Windows apps. Closing
  still hides it to the tray. Both can be changed in Settings.
- Renaming a download edits only the name. The extension shows beside it and
  stays the same.
- A name typed in the download dialog keeps the file's extension. A typed
  extension replaces one that was only guessed from the address.
- The on-video Download button stays inside the visible window.
- The installer uses the Downpour colors.

### Fixed

- Unfinished files are never overwritten by a stream rename.
- A failed save at startup is retried.
- A late background save could overwrite the download list after the app
  restarted in the same process.

### Upgrade note

- Download lists saved by 1.1.0 cannot be read by 1.0.0. Lists from 1.0.0 load
  in 1.1.0 as before.

## [1.0.0] - 2026-10-29

The first public release: a fast download manager for Windows 10 and 11, with
a Firefox add-on.

### Added

- Several connections per file, with pause, resume and crash recovery.
- Video streams (HLS and DASH) joined into one MP4, with a quality choice.
- A Download button on videos in Firefox, and a list of the media on each page.
- Takes over Firefox downloads with their cookies, in private windows and
  containers too.
- Queues, categories, speed limits, statistics and a link checker.
- Windows notifications, taskbar progress, tray icon, Start with Windows, and
  keeping the PC awake while downloading.
- A command line tool, `downpour-cli`.
- 10 color themes, with the Downpour theme as the default.

[1.1.0]: https://github.com/algometrix/downpour/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/algometrix/downpour/releases/tag/v1.0.0
