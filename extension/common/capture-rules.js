// Decisions shared by the background script, popup, and tests. Everything
// here is pure except the two small state keepers (createBypassList and
// createTabMediaLedger), which get their clock and storage from the caller.
// Loaded as a classic script (background scripts, extension pages) and as a
// CommonJS module by Node.

const DownpourRules = (() => {
  const DEFAULT_SETTINGS = Object.freeze({
    captureEnabled: true,
    fileTypes:
      "7z apk avi bz2 deb dmg epub exe flac gz iso m4a m4v mkv mov mp3 mp4 msi msix ogg pdf pkg rar rpm tar tgz wav webm wmv xz zip zst",
    minSizeKb: 0,
    excludedSites: "",
    videoButton: true,
    // Videos a server on this computer plays (a media server, a page under
    // development) are watched, not downloaded.
    captureLocalVideos: false,
    handOffNotice: true,
  });

  // What counts as a video for `captureLocalVideos`: video files and the
  // playlists of video streams.
  const VIDEO_EXTENSIONS = Object.freeze(["avi", "flv", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "webm", "wmv", "m3u8", "mpd"]);

  // Media smaller than this is usually a preview, ad, or adaptive-stream segment.
  const MIN_MEDIA_BYTES = 512 * 1024;

  const HLS_TYPES = ["application/vnd.apple.mpegurl", "application/x-mpegurl", "audio/mpegurl", "audio/x-mpegurl"];
  const DASH_TYPES = ["application/dash+xml"];
  // Pieces of adaptive streams; the stream's playlist is what gets downloaded.
  const SEGMENT_TYPES = ["video/mp2t", "video/iso.segment", "audio/iso.segment"];
  const SEGMENT_EXTENSIONS = ["ts", "m4s", "m4f", "cmfv", "cmfa"];

  function parseList(text) {
    return String(text ?? "")
      .split(/[\s,;]+/)
      .map((entry) => entry.trim().toLowerCase())
      .filter(Boolean);
  }

  function fileNameFromUrl(url) {
    try {
      const path = new URL(url).pathname;
      const last = path.split("/").filter(Boolean).pop() ?? "";
      return decodeURIComponent(last);
    } catch {
      return "";
    }
  }

  /** The last part of a path, without its folders. */
  function baseName(path) {
    return String(path ?? "").split(/[\\/]/).pop();
  }

  function fileExtension(fileName) {
    const base = baseName(fileName);
    const dot = base.lastIndexOf(".");
    return dot > 0 ? base.slice(dot + 1).toLowerCase() : "";
  }

  /// `example.com` matches the host and its subdomains; `*.example.com` only subdomains.
  function hostMatches(host, pattern) {
    const h = host.toLowerCase();
    if (pattern.startsWith("*.")) {
      return h.endsWith(pattern.slice(1));
    }
    return h === pattern || h.endsWith(`.${pattern}`);
  }

  /**
   * Decides whether a browser download should be handed to Downpour.
   * @param {{url: string, fileName?: string, sizeBytes?: number}} download
   * @returns {{capture: boolean, reason: string}}
   */
  function shouldCapture(download, settings) {
    const options = { ...DEFAULT_SETTINGS, ...settings };
    if (!options.captureEnabled) {
      return { capture: false, reason: "capture is turned off" };
    }
    let url;
    try {
      url = new URL(download.url);
    } catch {
      return { capture: false, reason: "invalid URL" };
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      // blob:, data: and file: downloads exist only inside the browser.
      return { capture: false, reason: `${url.protocol} links stay in the browser` };
    }
    if (parseList(options.excludedSites).some((pattern) => hostMatches(url.hostname, pattern))) {
      return { capture: false, reason: "site is excluded" };
    }
    const name = download.fileName || fileNameFromUrl(download.url);
    const extension = fileExtension(name);
    if (!options.captureLocalVideos && isLocalAddress(url.href) && VIDEO_EXTENSIONS.includes(extension)) {
      return { capture: false, reason: "videos from this computer stay in the browser" };
    }
    if (!parseList(options.fileTypes).includes(extension)) {
      return { capture: false, reason: extension ? `.${extension} is not a captured type` : "no file extension" };
    }
    const minBytes = Number(options.minSizeKb) * 1024;
    // A size of 0 means the browser does not know it yet.
    if (minBytes > 0 && Number.isFinite(download.sizeBytes) && download.sizeBytes > 0 && download.sizeBytes < minBytes) {
      return { capture: false, reason: "smaller than the minimum size" };
    }
    return { capture: true, reason: `.${extension} download` };
  }

  /** Formats cookies from `cookies.getAll` as a `Cookie` request header value. */
  function cookieHeader(cookies) {
    return (cookies ?? []).map((cookie) => `${cookie.name}=${cookie.value}`).join("; ");
  }

  function headerValue(headers, name) {
    const wanted = name.toLowerCase();
    const header = (headers ?? []).find((h) => h.name.toLowerCase() === wanted);
    return header?.value ?? "";
  }

  /** The response's type without parameters, lowercase; "" when absent. */
  function contentTypeOf(headers) {
    return headerValue(headers, "content-type").split(";")[0].trim().toLowerCase();
  }

  /** A byte count from a header, or null when it is missing or zero. */
  function positiveLength(text) {
    const length = Number(text);
    return text !== "" && Number.isFinite(length) && length > 0 ? length : null;
  }

  /**
   * Recognizes a response that is a downloadable media file.
   * @returns {{mime: string, sizeBytes: number | null} | null}
   */
  function mediaFromResponse(responseHeaders, statusCode, url = "") {
    if (statusCode !== 200 && statusCode !== 206) {
      return null;
    }
    const mime = contentTypeOf(responseHeaders);
    if (!mime.startsWith("video/") && !mime.startsWith("audio/")) {
      return null;
    }
    if (isStreamSegment(url, mime)) {
      return null;
    }
    const range = headerValue(responseHeaders, "content-range").match(/\/(\d+)\s*$/);
    const sizeBytes = positiveLength(range ? range[1] : headerValue(responseHeaders, "content-length"));
    if (sizeBytes !== null && sizeBytes < MIN_MEDIA_BYTES) {
      return null;
    }
    return { mime, sizeBytes };
  }

  // Types Firefox shows in a tab instead of saving; it saves any other type.
  const SHOWN_TYPE_PREFIXES = ["text/", "image/", "video/", "audio/"];
  const SHOWN_TYPES = [
    "application/pdf",
    "application/json",
    "application/xml",
    "application/xhtml+xml",
    "application/javascript",
    "application/x-javascript",
    "application/ecmascript",
    "application/rss+xml",
    "application/atom+xml",
    "application/manifest+json",
    "multipart/x-mixed-replace",
  ];

  function isShownInTab(mime) {
    return SHOWN_TYPES.includes(mime) || SHOWN_TYPE_PREFIXES.some((prefix) => mime.startsWith(prefix));
  }

  /** `filename*=UTF-8''...` (RFC 5987); other charsets are read as Latin-1. */
  function decodeExtendedValue(value) {
    const match = value.match(/^([^']*)'[^']*'(.*)$/);
    if (!match) {
      return "";
    }
    const [, charset, encoded] = match;
    try {
      if (charset.toLowerCase() === "utf-8") {
        return decodeURIComponent(encoded);
      }
      return encoded.replace(/%([0-9a-f]{2})/gi, (_, hex) => String.fromCharCode(parseInt(hex, 16)));
    } catch {
      return "";
    }
  }

  /**
   * Reads a Content-Disposition header (RFC 6266). `filename*` wins over
   * `filename`; any folders in the name are dropped.
   * @returns {{type: string, fileName: string}} type is "" when absent
   */
  function parseContentDisposition(header) {
    const value = String(header ?? "");
    const type = value.split(";")[0].trim().toLowerCase();
    const extended = value.match(/;\s*filename\*\s*=\s*([^;]+)/i);
    const plain = value.match(/;\s*filename\s*=\s*("(?:[^"\\]|\\.)*"|[^;]*)/i);
    const written = plain?.[1].trim() ?? "";
    const isQuoted = written.length >= 2 && written.startsWith('"') && written.endsWith('"');
    // A quote that is never closed is dropped, not the name's last letter.
    const fromPlain = isQuoted ? written.slice(1, -1).replace(/\\(.)/g, "$1") : written.replace(/^"/, "");
    const name = (extended && decodeExtendedValue(extended[1].trim())) || fromPlain;
    return { type, fileName: name.split(/[\\/]/).pop().trim() };
  }

  /** What to do with a page response, from `pageResponseDecision`. */
  const PAGE_RESPONSE = Object.freeze({
    // A page or anything else the tab shows: not a download.
    SHOWN: "shown",
    // A download the app can fetch again: hand it over.
    HAND_OVER: "hand-over",
    // A download only the browser can make: it keeps it, and no later
    // capture of the same address takes it either.
    BROWSER_KEEPS: "browser-keeps",
  });

  /**
   * Decides about a page response the browser may be about to save as a
   * file rather than show: one sent as an attachment, or of a type no tab
   * shows. A response without a type is shown, as Firefox looks at its
   * content first.
   * @param {{url: string, statusCode: number, method?: string, responseHeaders: Array}} response
   * @returns {{action: "shown"}
   *   | {action: "hand-over", download: {fileName: string, mime: string, sizeBytes: number | null}}
   *   | {action: "browser-keeps", reason: string}}
   */
  function pageResponseDecision(response) {
    if (response.statusCode !== 200) {
      return { action: PAGE_RESPONSE.SHOWN };
    }
    const headers = response.responseHeaders;
    const mime = contentTypeOf(headers);
    const disposition = parseContentDisposition(headerValue(headers, "content-disposition"));
    const isAttachment = disposition.type === "attachment";
    const isUnshownType = mime !== "" && !isShownInTab(mime);
    if (!isAttachment && !isUnshownType) {
      return { action: PAGE_RESPONSE.SHOWN };
    }
    // The app fetches the address again with GET. A form's POST answer
    // cannot be fetched that way, so the browser keeps it.
    const method = (response.method ?? "GET").toUpperCase();
    if (method !== "GET") {
      return { action: PAGE_RESPONSE.BROWSER_KEEPS, reason: `${method} response` };
    }
    return {
      action: PAGE_RESPONSE.HAND_OVER,
      download: {
        fileName: disposition.fileName || fileNameFromUrl(response.url),
        mime,
        sizeBytes: positiveLength(headerValue(headers, "content-length")),
      },
    };
  }

  function urlExtension(url) {
    try {
      return fileExtension(new URL(url).pathname);
    } catch {
      return "";
    }
  }

  /**
   * Recognizes an HLS playlist or DASH manifest response. CDNs often serve
   * playlists as text/plain or octet-stream, so the URL counts too.
   * @returns {"hls" | "dash" | null}
   */
  function streamFormatOf(url, responseHeaders) {
    const mime = contentTypeOf(responseHeaders);
    if (mime === "text/html") {
      return null;
    }
    const extension = urlExtension(url);
    if (HLS_TYPES.includes(mime) || extension === "m3u8") {
      return "hls";
    }
    if (DASH_TYPES.includes(mime) || extension === "mpd") {
      return "dash";
    }
    return null;
  }

  function isStreamSegment(url, mime) {
    return SEGMENT_TYPES.includes(mime) || SEGMENT_EXTENSIONS.includes(urlExtension(url));
  }

  /**
   * What a fetched playlist is. A multivariant HLS playlist lists the
   * qualities (it is the one to download); a media playlist lists the pieces
   * of one quality and is usually requested right after its multivariant.
   * @returns {{kind: "multivariant" | "media" | "dash" | "unknown", heights: number[], variantUrls: string[]}}
   */
  function describePlaylist(text, baseUrl) {
    const body = String(text ?? "");
    const heights = new Set();
    const variantUrls = [];
    const resolve = (reference) => {
      try {
        return new URL(reference, baseUrl).href;
      } catch {
        return null;
      }
    };
    if (body.trimStart().startsWith("#EXTM3U")) {
      const lines = body.split(/\r?\n/).map((line) => line.trim());
      let awaitingVariant = false;
      for (const line of lines) {
        if (line.startsWith("#EXT-X-STREAM-INF")) {
          const resolution = line.match(/RESOLUTION=\d+x(\d+)/i);
          if (resolution) heights.add(Number(resolution[1]));
          awaitingVariant = true;
        } else if (line.startsWith("#EXT-X-MEDIA")) {
          const uri = line.match(/URI="([^"]+)"/);
          const resolved = uri && resolve(uri[1]);
          if (resolved) variantUrls.push(resolved);
        } else if (awaitingVariant && line && !line.startsWith("#")) {
          const resolved = resolve(line);
          if (resolved) variantUrls.push(resolved);
          awaitingVariant = false;
        }
      }
      const kind = lines.some((line) => line.startsWith("#EXT-X-STREAM-INF")) ? "multivariant" : "media";
      return { kind, heights: [...heights].sort((a, b) => b - a), variantUrls };
    }
    if (/<MPD[\s>]/.test(body)) {
      for (const match of body.matchAll(/<Representation\b[^>]*\bheight="(\d+)"/g)) {
        heights.add(Number(match[1]));
      }
      return { kind: "dash", heights: [...heights].sort((a, b) => b - a), variantUrls };
    }
    return { kind: "unknown", heights: [], variantUrls };
  }

  /** `text` as an absolute http(s) URL, or null. */
  function parseHttpUrl(text) {
    let url;
    try {
      url = new URL(String(text ?? "").trim());
    } catch {
      return null; // relative or not a URL
    }
    return url.protocol === "http:" || url.protocol === "https:" ? url : null;
  }

  /** The origin of an absolute http(s) address, or null. */
  function httpOrigin(text) {
    return parseHttpUrl(text)?.origin ?? null;
  }

  /**
   * A download address from a message, checked before it reaches the app.
   * @returns {{ok: true, url: string} | {ok: false, reason: string}}
   */
  function parseDownloadUrl(text) {
    const url = parseHttpUrl(text);
    if (!url) {
      return { ok: false, reason: "Only http and https addresses can be downloaded." };
    }
    return { ok: true, url: url.href };
  }

  // Firefox's cookie store outside containers and private windows.
  const DEFAULT_COOKIE_STORE = "firefox-default";

  /** Whether `cookieStoreId` is the normal window's store (absent counts as it). */
  function isDefaultCookieStore(cookieStoreId) {
    return !cookieStoreId || cookieStoreId === DEFAULT_COOKIE_STORE;
  }

  /**
   * The `downloads.download` options that give a refused download back to
   * the browser in the session it came from: a private window's download
   * stays private, and a container's keeps its container's cookies.
   * @param {{url: string, finalUrl?: string, filename?: string, incognito?: boolean, cookieStoreId?: string}} item
   *   the browser's download item
   */
  function browserFallbackDownload(item) {
    const options = { url: item.finalUrl || item.url };
    const fileName = baseName(item.filename);
    if (fileName) {
      options.filename = fileName;
    }
    // Firefox rejects a container store together with incognito; the
    // private store follows from incognito alone.
    if (item.incognito === true) {
      options.incognito = true;
    } else if (!isDefaultCookieStore(item.cookieStoreId)) {
      options.cookieStoreId = item.cookieStoreId;
    }
    return options;
  }

  /**
   * Addresses no capture takes for a while: downloads given back to the
   * browser, which would otherwise be captured again as they start.
   * @param {number} ttlMs how long an address stays bypassed
   * @param {() => number} clock milliseconds now
   */
  function createBypassList(ttlMs, clock) {
    const until = new Map();
    function dropExpired(now) {
      for (const [url, end] of until) {
        if (now > end) until.delete(url);
      }
    }
    return {
      add(url) {
        const now = clock();
        dropExpired(now);
        until.set(url, now + ttlMs);
      },
      has(url) {
        const end = until.get(url);
        if (end === undefined) return false;
        if (clock() > end) {
          until.delete(url);
          return false;
        }
        return true;
      },
    };
  }

  /**
   * How often a tab's refused playlist may be read again: at most `limit`
   * times per address, `gapMs` apart. A tab's count starts over when it
   * shows a new page.
   * @param {{limit: number, gapMs: number, clock: () => number}} policy
   */
  function createRereadBudget(policy) {
    const tabs = new Map(); // tab id -> Map(address -> {count, at})
    return {
      /** Whether the playlist may be read now; counts the read when it may. */
      take(tabId, url) {
        const reads = tabs.get(tabId) ?? new Map();
        tabs.set(tabId, reads);
        const now = policy.clock();
        const last = reads.get(url) ?? { count: 0, at: -Infinity };
        if (last.count >= policy.limit || now - last.at < policy.gapMs) {
          return false;
        }
        reads.set(url, { count: last.count + 1, at: now });
        return true;
      },
      forget(tabId) {
        tabs.delete(tabId);
      },
    };
  }

  /**
   * Playlists read for videos that play them directly, so are not in the
   * tab's list; kept per tab until it shows a new page.
   */
  function createPlaylistMemo() {
    const tabs = new Map(); // tab id -> Map(address -> playlist)
    return {
      remember(tabId, url, playlist) {
        const playlists = tabs.get(tabId) ?? new Map();
        tabs.set(tabId, playlists);
        playlists.set(url, playlist);
      },
      /** The playlist read for the address, or undefined. */
      recall(tabId, url) {
        return tabs.get(tabId)?.get(url);
      },
      forget(tabId) {
        tabs.delete(tabId);
      },
    };
  }

  /** Whether `url` is listed in a tab's media, as an entry or a stream's variant. */
  function isListedMedia(list, url) {
    return list.some((entry) => entry.url === url || entry.variantUrls?.includes(url));
  }

  /**
   * A tab's media list with a new detection first, or null when it is
   * listed already. A player loads the multivariant playlist and then the
   * playlists of the qualities it picks; only the first is the download, so
   * a stream hides the variant playlists listed before it.
   * @param {Array} list the tab's media, newest first
   * @param {object} entry the detected file or stream
   * @param {number} limit how many entries a tab keeps
   */
  function withDetectedMedia(list, entry, limit) {
    if (isListedMedia(list, entry.url)) {
      return null;
    }
    const variants = entry.variantUrls ?? [];
    const others = list.filter((known) => !variants.includes(known.url));
    return [entry, ...others].slice(0, limit);
  }

  /**
   * A tab's media list after the stream at `url` was read again, or null when
   * the stream is gone or the read found nothing new. The playlist's
   * variants are hidden as on detection.
   * @param {Array} list the tab's media, newest first
   * @param {string} url the stream's address
   * @param {{kind: string, heights: number[], variantUrls: string[]}} playlist from `describePlaylist`
   */
  function withPlaylistRead(list, url, playlist) {
    const stream = list.find((entry) => entry.kind === "stream" && entry.url === url);
    if (!stream) {
      return null;
    }
    const unchanged = stream.playlistKind === playlist.kind && String(stream.heights) === String(playlist.heights);
    if (unchanged) {
      return null;
    }
    const variantUrls = playlist.kind === "multivariant" ? playlist.variantUrls : [];
    const updated = { ...stream, playlistKind: playlist.kind, heights: playlist.heights, variantUrls };
    return list
      .filter((entry) => !variantUrls.includes(entry.url))
      .map((entry) => (entry === stream ? updated : entry));
  }

  /**
   * Request headers for our own read of a page's playlist: the Referer and
   * Origin the page sent replace ours, because video CDNs often refuse
   * requests without them.
   * @param {Array<{name: string, value: string}>} headers what Firefox would send
   * @param {{sentReferer?: string, origin?: string}} page what the page sent
   */
  function withPageHeaders(headers, page) {
    const pageHeaders = [
      { name: "Referer", value: page.sentReferer },
      { name: "Origin", value: page.origin },
    ].filter((header) => header.value);
    const overridden = new Set(pageHeaders.map((header) => header.name.toLowerCase()));
    return [...(headers ?? []).filter((header) => !overridden.has(header.name.toLowerCase())), ...pageHeaders];
  }

  /**
   * Keeps each tab's media list right while detections overlap. Changes to
   * one tab run one at a time, each on a fresh read, so none is lost. A
   * change begun before the tab navigated is dropped, so a slow detection
   * never brings back the previous page's media.
   *
   * Navigation counts live in memory: when Firefox unloads the background
   * page, no change is in flight anyway.
   * @param {{read: (tabId: number) => Promise<Array>, write: (tabId: number, list: Array) => Promise<void>}} storage
   */
  function createTabMediaLedger(storage) {
    const navigations = new Map(); // tab id -> how often it navigated
    const tails = new Map(); // tab id -> the last queued change
    const navigationsOf = (tabId) => navigations.get(tabId) ?? 0;

    function queued(tabId, change) {
      const run = (tails.get(tabId) ?? Promise.resolve()).then(change);
      // A failed change must not block the next one; its caller still gets
      // the failure through `run`.
      const tail = run.then(
        () => {},
        () => {},
      );
      tails.set(tabId, tail);
      tail.then(() => {
        if (tails.get(tabId) === tail) tails.delete(tabId);
      });
      return run;
    }

    return {
      /** Marks the start of a detection in the tab; pass the result to `update`. */
      begin(tabId) {
        return { tabId, navigation: navigationsOf(tabId) };
      },
      /**
       * Applies `change` (the current list to the next one, or null for no
       * change) unless the tab navigated since `ticket`.
       * @returns {Promise<Array | null>} the list written, or null
       */
      update(ticket, change) {
        return queued(ticket.tabId, async () => {
          if (navigationsOf(ticket.tabId) !== ticket.navigation) return null;
          const next = change(await storage.read(ticket.tabId));
          if (!next) return null;
          await storage.write(ticket.tabId, next);
          return next;
        });
      },
      /** Runs `use` on the tab's list after the changes queued before it. */
      inspect(tabId, use) {
        return queued(tabId, async () => use(await storage.read(tabId)));
      },
      /** The tab shows a new page or closed: empties its list and drops older changes. */
      reset(tabId) {
        navigations.set(tabId, navigationsOf(tabId) + 1);
        return queued(tabId, () => storage.write(tabId, []));
      },
    };
  }

  /** What became of a download the app refused. */
  const REFUSAL_FALLBACK = Object.freeze({
    // Nothing else takes it (a link sent from a menu).
    NONE: "none",
    BROWSER_SAVES: "browser-saves",
    BROWSER_FAILED: "browser-failed",
  });

  const FALLBACK_SENTENCE = Object.freeze({
    [REFUSAL_FALLBACK.NONE]: "",
    [REFUSAL_FALLBACK.BROWSER_SAVES]: "The browser saves it instead.",
    [REFUSAL_FALLBACK.BROWSER_FAILED]: "The browser could not save it either.",
  });

  /** The name a handed-over download is shown by: its file's, else its page's title. */
  function handOffName(item) {
    return item.fileName || fileNameFromUrl(item.url) || item.title || "";
  }

  /**
   * The notice shown after downloads were handed to the app.
   * @param {{type: string, message?: string, count?: number}} reply the app's answer
   * @param {Array<{url: string, fileName?: string, title?: string}>} items what was sent
   * @param {{fallback: string}} refusal one of REFUSAL_FALLBACK: what became
   *   of the download if the app refused it
   * @returns {{outcome: "sent" | "refused", title: string, detail: string}}
   */
  function handOffNotice(reply, items, refusal) {
    if (reply?.type !== "accepted") {
      const detail = [reply?.message, FALLBACK_SENTENCE[refusal.fallback]].filter(Boolean).join(" ");
      return { outcome: "refused", title: "Downpour did not take it", detail };
    }
    const count = reply.count ?? items.length;
    const detail = count === 1 ? handOffName(items[0] ?? {}) || "1 download" : `${count} downloads`;
    return { outcome: "sent", title: "Sent to Downpour", detail };
  }

  /**
   * Whether two addresses belong to the same site (scheme, host, and port).
   * A notice naming a download is drawn only into the site it came from,
   * since the page can see what is added to it.
   */
  function sameOrigin(first, second) {
    const origin = httpOrigin(first);
    return origin !== null && origin === httpOrigin(second);
  }

  /**
   * The page a download was started from. Many sites send only their origin
   * as the Referer (the "strict-origin" referrer policies), which loses the
   * page; the tab that started the download still shows it. A full Referer
   * wins, since the tab may have moved on, and the tab counts only while it
   * shows the Referer's site, so an unrelated page is never recorded.
   */
  function downloadPage(referrer, tabPage) {
    const sent = parseHttpUrl(referrer);
    if (!sent) return undefined;
    const sentOnlyTheSite = sent.pathname === "/" && !sent.search;
    const shown = parseHttpUrl(tabPage);
    if (sentOnlyTheSite && shown && shown.origin === sent.origin) return shown.href;
    return sent.href;
  }

  /** The title to name a video found on a page after, or undefined when it has none. */
  function videoTitle(title) {
    const trimmed = String(title ?? "").trim();
    return trimmed || undefined;
  }

  /**
   * The title of the page that started a download, from `tab`: it counts
   * only while the tab shows the Referer's site. A link opened in a new tab
   * would otherwise name the video "New Tab".
   */
  function startingPageTitle(tab, referrer) {
    return sameOrigin(tab?.url, referrer) ? videoTitle(tab.title) : undefined;
  }

  /** Whether a media response is a video; audio is not what the video settings are about. */
  function isVideoMime(mime) {
    return String(mime ?? "").toLowerCase().startsWith("video/");
  }

  /**
   * The title to name a link from a page after. A page lists many links
   * (episodes, parts), so its title names only a linked stream, whose own
   * name (`master.m3u8`) says nothing; a linked file keeps its name.
   */
  function linkTitle(linkUrl, title) {
    return streamFormatOf(linkUrl, []) ? videoTitle(title) : undefined;
  }

  /**
   * Whether `text` is an address of this computer: localhost (and its
   * subdomains), the 127.0.0.0/8 loopback range, ::1, or 0.0.0.0. The URL
   * parser has already written short forms such as 127.1 out in full.
   */
  function isLocalAddress(text) {
    const url = parseHttpUrl(text);
    if (!url) return false;
    const host = url.hostname.toLowerCase();
    return (
      host === "localhost" ||
      host.endsWith(".localhost") ||
      /^127\.\d+\.\d+\.\d+$/.test(host) ||
      host === "[::1]" ||
      host === "0.0.0.0"
    );
  }

  /**
   * Whether a video found on `pageUrl` at `videoUrl` is left alone: it comes
   * from, or plays on, this computer and the settings keep such videos in
   * the browser.
   */
  function isIgnoredLocalVideo(videoUrl, pageUrl, settings) {
    if ({ ...DEFAULT_SETTINGS, ...settings }.captureLocalVideos) return false;
    return isLocalAddress(videoUrl) || isLocalAddress(pageUrl);
  }

  /**
   * What the on-video Download button sends for a <video>. A plain media
   * address is used as is. Streaming players play from a blob: address, so
   * the stream detected in the tab is used instead, preferring one the same
   * frame loaded (pages often embed the player in an iframe).
   * @param {Array} entries detected media for the tab, newest first
   * @param {{src: string, frameId: number}} video
   * @returns {object | null} a media entry, or null when nothing is known yet
   */
  function pickVideoSource(entries, video) {
    const src = String(video.src ?? "");
    if (/^https?:/i.test(src)) {
      const format = streamFormatOf(src, []);
      const known = (entries ?? []).find((entry) => entry.url === src);
      if (known) return known;
      return format ? { kind: "stream", format, url: src } : { kind: "file", url: src, fileName: fileNameFromUrl(src) };
    }
    const candidates = frameStreams(entries, video.frameId);
    // A player that switches quality loads another one-quality playlist
    // after the multivariant one; the one listing the qualities is the download.
    return candidates.find(listsQualities) ?? candidates[0] ?? null;
  }

  /** The tab's streams from the frame, or all of them when the frame has none; newest first. */
  function frameStreams(entries, frameId) {
    const streams = (entries ?? []).filter((entry) => entry.kind === "stream");
    const inFrame = streams.filter((entry) => entry.frameId === frameId);
    return inFrame.length > 0 ? inFrame : streams;
  }

  function listsQualities(entry) {
    return entry.heights?.length > 0;
  }

  /** A stream whose playlist was never read (a video whose address is it) or whose read failed. */
  function needsPlaylistRead(entry) {
    return entry?.kind === "stream" && (entry.playlistKind === undefined || entry.playlistKind === "unknown");
  }

  /**
   * The stream of the frame whose playlist should be read again, or null.
   * When the server refused the multivariant playlist, its variants were
   * not hidden and are listed after it; the player loads the multivariant
   * first, so the oldest unread stream is the one that lists the qualities.
   */
  function streamToReread(entries, frameId) {
    const candidates = frameStreams(entries, frameId);
    if (candidates.some(listsQualities)) {
      return null;
    }
    return candidates.findLast(needsPlaylistRead) ?? null;
  }

  /** Whether `source` was seen loading in the frame, not just guessed from a video's address. */
  function isDetectedInFrame(entries, source, frameId) {
    return Boolean(source) && (entries ?? []).some((entry) => entry.url === source.url && entry.frameId === frameId);
  }

  /** The frame's address as a referrer, or the page's when the frame has none of its own (about:blank). */
  function frameReferrer(frameUrl, pageUrl) {
    return /^https?:/i.test(frameUrl ?? "") ? frameUrl : pageUrl;
  }

  /** "1080p, 720p, 480p" for the popup; empty when unknown. */
  function qualitySummary(heights) {
    return (heights ?? []).map((height) => `${height}p`).join(", ");
  }

  const BEST_QUALITY = Object.freeze({ kind: "best" });

  // The app reads the height as an unsigned 32-bit number.
  const MAX_PIXELS = 4_294_967_295;

  /**
   * A quality from the video button, checked before it goes to the app.
   * Absent means the best quality.
   * @returns {{ok: true, quality: object} | {ok: false, reason: string}}
   */
  function parseQuality(value) {
    if (value === undefined || value?.kind === "best") {
      return { ok: true, quality: BEST_QUALITY };
    }
    const pixels = value?.pixels;
    if (value?.kind === "max_height" && Number.isInteger(pixels) && pixels >= 1 && pixels <= MAX_PIXELS) {
      return { ok: true, quality: { kind: "max_height", pixels } };
    }
    return { ok: false, reason: `unsupported video quality ${JSON.stringify(value)}` };
  }

  /**
   * The choices of the video button's menu, best first. The app reads the
   * same `quality` objects (`{kind: "best"}`, `{kind: "max_height", pixels}`).
   * The tallest height is the best choice itself, so it is not listed twice.
   * @param {number[]} heights the stream's heights, in any order
   * @returns {Array<{label: string, height: number | undefined, quality: object}>}
   */
  function videoQualityChoices(heights) {
    const distinct = [...new Set((heights ?? []).filter((height) => Number.isInteger(height) && height > 0))]
      .sort((a, b) => b - a);
    const [tallest, ...lower] = distinct;
    const best = { label: tallest ? `Best (${tallest}p)` : "Best quality", height: tallest, quality: BEST_QUALITY };
    const others = lower.map((pixels) => ({ label: `${pixels}p`, height: pixels, quality: { kind: "max_height", pixels } }));
    return [best, ...others];
  }

  function formatBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes < 0) {
      return "size unknown";
    }
    const units = ["B", "KB", "MB", "GB", "TB"];
    let value = bytes;
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) {
      value /= 1024;
      unit += 1;
    }
    return unit === 0 ? `${bytes} B` : `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
  }

  // ---- The on-video button's placement and pinning ----
  // video-button.js reads the page and moves the button; these decide.

  // Smaller videos are previews, ads, or thumbnails.
  const MIN_VIDEO_BUTTON_WIDTH = 200;
  const MIN_VIDEO_BUTTON_HEIGHT = 120;

  /** Whether a box is large enough to be a video worth a button. */
  function isLargeBox(rect) {
    return rect.width >= MIN_VIDEO_BUTTON_WIDTH && rect.height >= MIN_VIDEO_BUTTON_HEIGHT;
  }

  /**
   * Which box the button sits in, given the video's box and the boxes of the
   * elements around it, nearest first: the video itself (-1), the index of
   * the surrounding box to use, or null. A video the player keeps hidden (no
   * size at all) until it plays takes the player's box around it; a small
   * video is a preview or a thumbnail and gets none.
   */
  function videoBoxChoice(videoRect, surroundingRects) {
    if (isLargeBox(videoRect)) return -1;
    if (videoRect.width > 0 && videoRect.height > 0) return null;
    const index = surroundingRects.findIndex(isLargeBox);
    return index === -1 ? null : index;
  }

  /** The index of the largest box, or -1 when there is none; the first wins a tie. */
  function largestBox(rects) {
    let largest = -1;
    let largestArea = 0;
    rects.forEach((rect, index) => {
      const area = rect.width * rect.height;
      if (area > largestArea) {
        largest = index;
        largestArea = area;
      }
    });
    return largest;
  }

  /**
   * Where the button goes: the video's top-right corner, kept inside the
   * visible width, so a video wider than the window or cut off on the right
   * neither hides the button nor widens the page. `rect` is in window
   * coordinates; `scroll` turns them into page coordinates (zero in
   * fullscreen, where the button is fixed to the screen).
   * @returns {{left: number, top: number}}
   */
  function videoButtonPosition(rect, { width, visibleWidth, scroll, gap }) {
    const right = Math.min(rect.right, visibleWidth);
    const insideVideo = Math.max(rect.left, right - width - gap);
    const left = Math.max(gap, Math.min(insideVideo, visibleWidth - width - gap));
    return { left: left + scroll.x, top: rect.top + gap + scroll.y };
  }

  /**
   * Whether the button hides after the pointer leaves its video. It stays
   * while its menu is open, and on the frame's main video, except in
   * fullscreen, where it hides like the player's own controls.
   */
  function videoButtonHidesWhenLeft({ menuOpen, onMainVideo, fullscreen }) {
    if (menuOpen) return false;
    return !onMainVideo || fullscreen;
  }

  /** Whether the button goes back to the main video after leaving `another` video. */
  function videoButtonReturnsToMain({ leftMainVideo, fullscreen }) {
    return !leftMainVideo && !fullscreen;
  }

  /**
   * What the button does when fullscreen starts or ends.
   * @returns {"hide-later" | "stay" | "show-main" | "nothing"}
   */
  function videoButtonOnFullscreen({ fullscreen, showing, onMainVideo }) {
    if (fullscreen) return "hide-later";
    if (showing && onMainVideo) return "stay";
    if (!showing) return "show-main";
    return "nothing";
  }

  return {
    isLargeBox,
    videoBoxChoice,
    largestBox,
    videoButtonPosition,
    videoButtonHidesWhenLeft,
    videoButtonReturnsToMain,
    videoButtonOnFullscreen,
    DEFAULT_SETTINGS,
    parseList,
    fileNameFromUrl,
    baseName,
    fileExtension,
    hostMatches,
    shouldCapture,
    cookieHeader,
    headerValue,
    mediaFromResponse,
    parseContentDisposition,
    PAGE_RESPONSE,
    pageResponseDecision,
    streamFormatOf,
    describePlaylist,
    qualitySummary,
    parseQuality,
    videoQualityChoices,
    pickVideoSource,
    needsPlaylistRead,
    streamToReread,
    isDetectedInFrame,
    frameReferrer,
    REFUSAL_FALLBACK,
    handOffNotice,
    sameOrigin,
    downloadPage,
    videoTitle,
    startingPageTitle,
    isVideoMime,
    linkTitle,
    isLocalAddress,
    isIgnoredLocalVideo,
    parseDownloadUrl,
    isDefaultCookieStore,
    browserFallbackDownload,
    createBypassList,
    createRereadBudget,
    createPlaylistMemo,
    isListedMedia,
    withDetectedMedia,
    withPlaylistRead,
    withPageHeaders,
    createTabMediaLedger,
    formatBytes,
  };
})();

if (typeof module !== "undefined") {
  module.exports = DownpourRules;
}
