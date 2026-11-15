// Run with: node --test extension/common/capture-rules.test.js
const test = require("node:test");
const assert = require("node:assert/strict");
const rules = require("./capture-rules.js");

test("captures configured file types", () => {
  assert.equal(rules.shouldCapture({ url: "https://example.com/setup.EXE" }, {}).capture, true);
  assert.equal(rules.shouldCapture({ url: "https://example.com/page.html" }, {}).capture, false);
});

test("browser-provided file name wins over the URL", () => {
  const download = { url: "https://example.com/download?id=7", fileName: "C:\\Users\\me\\Downloads\\report.pdf" };
  assert.equal(rules.shouldCapture(download, {}).capture, true);
});

test("leaves blob and data downloads to the browser", () => {
  assert.equal(rules.shouldCapture({ url: "blob:https://example.com/1234", fileName: "a.zip" }, {}).capture, false);
  assert.equal(rules.shouldCapture({ url: "data:application/zip;base64,AA", fileName: "a.zip" }, {}).capture, false);
});

test("respects capture toggle, minimum size, and excluded sites", () => {
  const url = "https://cdn.example.com/a.zip";
  assert.equal(rules.shouldCapture({ url }, { captureEnabled: false }).capture, false);
  assert.equal(rules.shouldCapture({ url, sizeBytes: 1000 }, { minSizeKb: 10 }).capture, false);
  assert.equal(rules.shouldCapture({ url, sizeBytes: -1 }, { minSizeKb: 10 }).capture, true, "unknown size is captured");
  assert.equal(rules.shouldCapture({ url, sizeBytes: 0 }, { minSizeKb: 10 }).capture, true, "0 is a size not known yet");
  assert.equal(rules.shouldCapture({ url }, { excludedSites: "example.com" }).capture, false);
  assert.equal(rules.shouldCapture({ url }, { excludedSites: "example.net, *.example.org" }).capture, true);
});

test("host patterns", () => {
  assert.equal(rules.hostMatches("example.com", "example.com"), true);
  assert.equal(rules.hostMatches("a.example.com", "example.com"), true);
  assert.equal(rules.hostMatches("badexample.com", "example.com"), false);
  assert.equal(rules.hostMatches("example.com", "*.example.com"), false);
  assert.equal(rules.hostMatches("a.example.com", "*.example.com"), true);
});

test("cookie header", () => {
  assert.equal(rules.cookieHeader([{ name: "a", value: "1" }, { name: "b", value: "2" }]), "a=1; b=2");
  assert.equal(rules.cookieHeader([]), "");
});

test("media detection", () => {
  const headers = (pairs) => Object.entries(pairs).map(([name, value]) => ({ name, value }));
  assert.deepEqual(rules.mediaFromResponse(headers({ "Content-Type": "video/mp4", "Content-Length": "5000000" }), 200), {
    mime: "video/mp4",
    sizeBytes: 5000000,
  });
  assert.equal(
    rules.mediaFromResponse(headers({ "Content-Type": "video/mp4", "Content-Range": "bytes 0-1/9000000" }), 206).sizeBytes,
    9000000,
  );
  assert.equal(rules.mediaFromResponse(headers({ "Content-Type": "video/mp2t", "Content-Length": "40000" }), 200), null);
  assert.equal(rules.mediaFromResponse(headers({ "Content-Type": "text/html" }), 200), null);
  assert.equal(rules.mediaFromResponse(headers({ "Content-Type": "audio/mpeg" }), 404), null);
});

test("file names and sizes", () => {
  assert.equal(rules.fileNameFromUrl("https://example.com/x/My%20File.zip?y=1"), "My File.zip");
  assert.equal(rules.fileExtension("archive.tar.gz"), "gz");
  assert.equal(rules.fileExtension(".bashrc"), "");
  assert.equal(rules.formatBytes(1536), "1.5 KB");
  assert.equal(rules.formatBytes(null), "size unknown");
});

test("recognizes stream manifests by type or URL, never web pages", () => {
  const type = (value) => [{ name: "Content-Type", value }];
  assert.equal(rules.streamFormatOf("https://cdn.example/x/master.m3u8?t=1", type("text/plain")), "hls");
  assert.equal(rules.streamFormatOf("https://cdn.example/x/playlist", type("application/vnd.apple.mpegurl")), "hls");
  assert.equal(rules.streamFormatOf("https://cdn.example/x/manifest.mpd", type("application/octet-stream")), "dash");
  assert.equal(rules.streamFormatOf("https://cdn.example/x/list", type("application/dash+xml; charset=utf-8")), "dash");
  assert.equal(rules.streamFormatOf("https://example.org/watch.m3u8", type("text/html")), null);
  assert.equal(rules.streamFormatOf("https://cdn.example/x/video.mp4", type("video/mp4")), null);
});

test("stream pieces are not listed as media", () => {
  const headers = (mime, length) => [
    { name: "Content-Type", value: mime },
    { name: "Content-Length", value: String(length) },
  ];
  assert.equal(rules.mediaFromResponse(headers("video/mp2t", 2_000_000), 200, "https://cdn.example/seg_001.ts"), null);
  assert.equal(rules.mediaFromResponse(headers("video/mp4", 2_000_000), 206, "https://cdn.example/chunk-5.m4s"), null);
  assert.notEqual(rules.mediaFromResponse(headers("video/mp4", 2_000_000), 200, "https://cdn.example/movie.mp4"), null);
});

test("describes multivariant, media, and DASH playlists", () => {
  const multivariant = [
    "#EXTM3U",
    '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",NAME="en",URI="audio/en.m3u8"',
    "#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080",
    "1080/index.m3u8",
    "#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=854x480",
    "https://other.example/480.m3u8",
  ].join("\n");
  const described = rules.describePlaylist(multivariant, "https://cdn.example/v/master.m3u8");
  assert.equal(described.kind, "multivariant");
  assert.deepEqual(described.heights, [1080, 480]);
  assert.deepEqual(described.variantUrls, [
    "https://cdn.example/v/audio/en.m3u8",
    "https://cdn.example/v/1080/index.m3u8",
    "https://other.example/480.m3u8",
  ]);
  assert.equal(rules.describePlaylist("#EXTM3U\n#EXTINF:4,\nseg0.ts\n", "https://cdn.example/").kind, "media");
  const mpd = '<MPD><Period><AdaptationSet><Representation id="v" height="720"/><Representation height="1080"/></AdaptationSet></Period></MPD>';
  assert.deepEqual(rules.describePlaylist(mpd, "https://cdn.example/").heights, [1080, 720]);
  assert.equal(rules.describePlaylist("<html>", "https://cdn.example/").kind, "unknown");
  assert.equal(rules.qualitySummary([1080, 720]), "1080p, 720p");
});

test("the video button picks the frame's stream for blob players", () => {
  const entries = [
    { kind: "stream", format: "hls", url: "https://cdn.example/ad/master.m3u8", frameId: 0 },
    { kind: "file", url: "https://cdn.example/trailer.mp4", frameId: 0 },
    { kind: "stream", format: "hls", url: "https://cdn.example/movie/master.m3u8", frameId: 7 },
  ];
  assert.equal(rules.pickVideoSource(entries, { src: "blob:https://example.org/1", frameId: 7 }).url, "https://cdn.example/movie/master.m3u8");
  assert.equal(rules.pickVideoSource(entries, { src: "blob:https://example.org/1", frameId: 3 }).url, "https://cdn.example/ad/master.m3u8");
  assert.equal(rules.pickVideoSource([], { src: "blob:x", frameId: 0 }), null);
});

test("the video button prefers the frame's stream that lists qualities", () => {
  const entries = [
    { kind: "stream", format: "hls", url: "https://cdn.example/v/720.m3u8?t=2", heights: [], frameId: 7 },
    { kind: "stream", format: "hls", url: "https://cdn.example/v/master.m3u8", heights: [1080, 720], frameId: 7 },
    { kind: "stream", format: "hls", url: "https://cdn.example/ad/master.m3u8", heights: [480], frameId: 0 },
  ];
  assert.equal(rules.pickVideoSource(entries, { src: "blob:x", frameId: 7 }).url, "https://cdn.example/v/master.m3u8");
  const unread = entries.slice(0, 1);
  assert.equal(rules.pickVideoSource(unread, { src: "blob:x", frameId: 7 }).url, "https://cdn.example/v/720.m3u8?t=2");
});

test("the video button uses plain media addresses directly", () => {
  assert.deepEqual(rules.pickVideoSource([], { src: "https://cdn.example/v/clip.mp4", frameId: 0 }), {
    kind: "file",
    url: "https://cdn.example/v/clip.mp4",
    fileName: "clip.mp4",
  });
  assert.equal(rules.pickVideoSource([], { src: "https://cdn.example/v/index.m3u8", frameId: 0 }).kind, "stream");
});

test("the video button offers the best quality first, then each lower height once", () => {
  assert.deepEqual(rules.videoQualityChoices([720, 1080, 480, 720]), [
    { label: "Best (1080p)", height: 1080, quality: { kind: "best" } },
    { label: "720p", height: 720, quality: { kind: "max_height", pixels: 720 } },
    { label: "480p", height: 480, quality: { kind: "max_height", pixels: 480 } },
  ]);
  assert.deepEqual(rules.videoQualityChoices([]), [{ label: "Best quality", height: undefined, quality: { kind: "best" } }]);
  assert.equal(rules.videoQualityChoices(undefined).length, 1);
});

test("reads the file name from Content-Disposition", () => {
  assert.deepEqual(rules.parseContentDisposition('attachment; filename="My File.zip"'), { type: "attachment", fileName: "My File.zip" });
  assert.deepEqual(rules.parseContentDisposition("Attachment; filename=plain.iso"), { type: "attachment", fileName: "plain.iso" });
  assert.equal(
    rules.parseContentDisposition("attachment; filename=\"fallback.zip\"; filename*=UTF-8''%E2%82%AC%20rates.zip").fileName,
    "€ rates.zip",
    "filename* wins",
  );
  assert.equal(rules.parseContentDisposition("attachment; filename*=iso-8859-1'en'%E9t%E9.zip").fileName, "été.zip");
  assert.equal(rules.parseContentDisposition('attachment; filename="..\\\\..\\\\evil.exe"').fileName, "evil.exe");
  assert.equal(rules.parseContentDisposition('attachment; filename="say \\"hi\\".zip"').fileName, 'say "hi".zip');
  assert.equal(rules.parseContentDisposition('attachment; filename="report.zip').fileName, "report.zip", "unclosed quote");
  assert.equal(rules.parseContentDisposition("attachment; filename*=UTF-8''%E2%28bad.zip").fileName, "", "undecodable");
  assert.deepEqual(rules.parseContentDisposition(""), { type: "", fileName: "" });
  assert.deepEqual(rules.parseContentDisposition("inline"), { type: "inline", fileName: "" });
});

test("a page response is a download when sent as an attachment or of a type no tab shows", () => {
  const { SHOWN, HAND_OVER } = rules.PAGE_RESPONSE;
  const response = (headers, extra = {}) => ({
    url: "https://files.example/get/setup.exe?token=1",
    statusCode: 200,
    method: "GET",
    responseHeaders: Object.entries(headers).map(([name, value]) => ({ name, value })),
    ...extra,
  });
  assert.deepEqual(rules.pageResponseDecision(response({ "Content-Type": "application/octet-stream", "Content-Length": "2048" })), {
    action: HAND_OVER,
    download: { fileName: "setup.exe", mime: "application/octet-stream", sizeBytes: 2048 },
  });
  assert.deepEqual(
    rules.pageResponseDecision(response({ "Content-Type": "application/pdf", "Content-Disposition": 'attachment; filename="r.pdf"' })),
    { action: HAND_OVER, download: { fileName: "r.pdf", mime: "application/pdf", sizeBytes: null } },
  );
  // Shown in the tab: web pages, PDFs, and videos the browser plays itself.
  for (const shown of ["text/html; charset=utf-8", "application/pdf", "video/mp4", "image/png", "application/json"]) {
    assert.deepEqual(rules.pageResponseDecision(response({ "Content-Type": shown })), { action: SHOWN }, shown);
  }
  assert.deepEqual(rules.pageResponseDecision(response({ "Content-Disposition": "inline" })), { action: SHOWN }, "no type: Firefox sniffs");
  assert.deepEqual(rules.pageResponseDecision(response({ "Content-Type": "application/zip" }, { statusCode: 404 })), { action: SHOWN });
});

test("a POST download stays with the browser, and a POST page is just a page", () => {
  const { SHOWN, BROWSER_KEEPS } = rules.PAGE_RESPONSE;
  const post = (contentType) => ({
    url: "https://files.example/export",
    statusCode: 200,
    method: "post",
    responseHeaders: [{ name: "Content-Type", value: contentType }],
  });
  // The background bypasses capture for this address, so onCreated does not
  // cancel the browser's copy and send the app an address it cannot fetch.
  assert.deepEqual(rules.pageResponseDecision(post("application/zip")), { action: BROWSER_KEEPS, reason: "POST response" });
  assert.deepEqual(rules.pageResponseDecision(post("text/html")), { action: SHOWN }, "a form's result page");
});

test("a bypassed address stays bypassed until its time runs out", () => {
  let now = 1_000;
  const bypassed = rules.createBypassList(60_000, () => now);
  bypassed.add("https://cdn.example/a.zip");
  assert.equal(bypassed.has("https://cdn.example/a.zip"), true);
  assert.equal(bypassed.has("https://cdn.example/b.zip"), false);
  now += 60_000;
  assert.equal(bypassed.has("https://cdn.example/a.zip"), true, "the last millisecond still counts");
  now += 1;
  assert.equal(bypassed.has("https://cdn.example/a.zip"), false);
  bypassed.add("https://cdn.example/a.zip");
  assert.equal(bypassed.has("https://cdn.example/a.zip"), true, "added again after expiring");
});

test("a refused download goes back to the browser in its own window and container", () => {
  const item = { url: "https://cdn.example/get?id=1", finalUrl: "https://cdn.example/a.zip", filename: "C:\\Downloads\\a.zip" };
  assert.deepEqual(rules.browserFallbackDownload(item), { url: "https://cdn.example/a.zip", filename: "a.zip" });
  assert.deepEqual(rules.browserFallbackDownload({ ...item, incognito: true, cookieStoreId: "firefox-private" }), {
    url: "https://cdn.example/a.zip",
    filename: "a.zip",
    incognito: true,
  });
  assert.deepEqual(rules.browserFallbackDownload({ ...item, incognito: false, cookieStoreId: "firefox-container-2" }), {
    url: "https://cdn.example/a.zip",
    filename: "a.zip",
    cookieStoreId: "firefox-container-2",
  });
  assert.deepEqual(rules.browserFallbackDownload({ url: "https://cdn.example/x", cookieStoreId: "firefox-default", filename: "" }), {
    url: "https://cdn.example/x",
  });
});

test("only absolute http(s) download addresses pass", () => {
  assert.deepEqual(rules.parseDownloadUrl(" https://example.com/a.zip "), { ok: true, url: "https://example.com/a.zip" });
  for (const bad of ["javascript:alert(1)", "file:///C:/a.zip", "/relative.zip", undefined, 7]) {
    assert.equal(rules.parseDownloadUrl(bad).ok, false, String(bad));
  }
});

test("a detection is listed once, newest first, and a stream hides its variants", () => {
  const variant = { kind: "stream", url: "https://cdn.example/v/720.m3u8", variantUrls: [] };
  const file = { kind: "file", url: "https://cdn.example/clip.mp4" };
  const master = {
    kind: "stream",
    url: "https://cdn.example/v/master.m3u8",
    variantUrls: ["https://cdn.example/v/720.m3u8", "https://cdn.example/v/480.m3u8"],
  };
  assert.deepEqual(rules.withDetectedMedia([variant, file], master, 25), [master, file]);
  assert.equal(rules.withDetectedMedia([master], file, 25).length, 2);
  assert.equal(rules.withDetectedMedia([master, file], file, 25), null, "listed already");
  assert.equal(rules.withDetectedMedia([master], { ...variant }, 25), null, "a listed stream's variant");
  assert.deepEqual(rules.withDetectedMedia([file], master, 1), [master], "the oldest falls off");
});

/** In-memory storage whose reads and writes take a turn of the event loop, like browser storage. */
function slowStorage() {
  const lists = new Map();
  const tick = () => new Promise((resolve) => setTimeout(resolve, 1));
  return {
    lists,
    read: async (tabId) => {
      await tick();
      return lists.get(tabId) ?? [];
    },
    write: async (tabId, list) => {
      await tick();
      lists.set(tabId, list);
    },
  };
}

test("overlapping detections in a tab are all kept", async () => {
  const storage = slowStorage();
  const ledger = rules.createTabMediaLedger(storage);
  const urls = ["https://cdn.example/1.mp4", "https://cdn.example/2.mp4", "https://cdn.example/3.mp4"];
  await Promise.all(
    urls.map((url) => ledger.update(ledger.begin(7), (list) => rules.withDetectedMedia(list, { kind: "file", url }, 25))),
  );
  assert.deepEqual(storage.lists.get(7).map((entry) => entry.url).sort(), urls);
});

test("a detection begun before the tab navigated does not bring back the old page's media", async () => {
  const storage = slowStorage();
  const ledger = rules.createTabMediaLedger(storage);
  const slow = ledger.begin(7); // e.g. waiting on a playlist read
  await ledger.update(ledger.begin(7), () => [{ kind: "file", url: "https://cdn.example/old.mp4" }]);
  await ledger.reset(7); // the tab navigated
  assert.equal(await ledger.update(slow, () => [{ kind: "file", url: "https://cdn.example/late.mp4" }]), null);
  assert.deepEqual(storage.lists.get(7), []);
  const written = await ledger.update(ledger.begin(7), () => [{ kind: "file", url: "https://cdn.example/new.mp4" }]);
  assert.equal(written.length, 1, "detections on the new page are kept");
  assert.equal(storage.lists.get(8), undefined, "other tabs are untouched");
});

test("a read for the badge sees the changes queued before it", async () => {
  const storage = slowStorage();
  const ledger = rules.createTabMediaLedger(storage);
  const adding = ledger.update(ledger.begin(5), () => [{ kind: "file", url: "https://cdn.example/a.mp4" }]);
  const counted = ledger.inspect(5, (list) => list.length);
  await adding;
  assert.equal(await counted, 1);
});

test("a failed change does not block the tab's next change", async () => {
  const storage = slowStorage();
  const ledger = rules.createTabMediaLedger(storage);
  const failing = ledger.update(ledger.begin(3), () => {
    throw new Error("storage full");
  });
  await assert.rejects(failing, /storage full/);
  const written = await ledger.update(ledger.begin(3), () => [{ kind: "file", url: "https://cdn.example/a.mp4" }]);
  assert.equal(written.length, 1);
});

test("only qualities the app can read are sent; absent means best", () => {
  const best = { ok: true, quality: { kind: "best" } };
  assert.deepEqual(rules.parseQuality(undefined), best);
  assert.deepEqual(rules.parseQuality({ kind: "best" }), best);
  assert.deepEqual(rules.parseQuality({ kind: "max_height", pixels: 720 }), {
    ok: true,
    quality: { kind: "max_height", pixels: 720 },
  });
  for (const bad of [null, { kind: "max_height", pixels: 0 }, { kind: "max_height", pixels: 7.5 },
    { kind: "max_height", pixels: 2 ** 32 }, { kind: "max_height" }, { kind: "worst" }]) {
    assert.equal(rules.parseQuality(bad).ok, false, JSON.stringify(bad));
  }
});

test("the hand-off notice names the file, or counts several", () => {
  const { NONE, BROWSER_SAVES, BROWSER_FAILED } = rules.REFUSAL_FALLBACK;
  const sent = { type: "accepted", count: 1 };
  assert.deepEqual(rules.handOffNotice(sent, [{ url: "https://cdn.example/x", fileName: "movie.part1.rar" }], { fallback: NONE }), {
    outcome: "sent",
    title: "Sent to Downpour",
    detail: "movie.part1.rar",
  });
  assert.equal(rules.handOffNotice(sent, [{ url: "https://cdn.example/files/setup.zip" }], { fallback: NONE }).detail, "setup.zip", "name from the address");
  assert.equal(rules.handOffNotice(sent, [{ url: "https://cdn.example/", title: "Episode 4" }], { fallback: NONE }).detail, "Episode 4", "the title last");
  assert.equal(rules.handOffNotice(sent, [{ url: "https://cdn.example/" }], { fallback: NONE }).detail, "1 download", "no name known");
  const three = [{ url: "https://a.example/1.zip" }, { url: "https://a.example/2.zip" }, { url: "https://a.example/3.zip" }];
  assert.equal(rules.handOffNotice({ type: "accepted", count: 3 }, three, { fallback: NONE }).detail, "3 downloads");

  const refusal = { type: "error", message: "Register browser integration in Downpour." };
  const item = [{ url: "https://cdn.example/a.zip" }];
  assert.deepEqual(rules.handOffNotice(refusal, item, { fallback: BROWSER_SAVES }), {
    outcome: "refused",
    title: "Downpour did not take it",
    detail: "Register browser integration in Downpour. The browser saves it instead.",
  });
  assert.equal(
    rules.handOffNotice(refusal, item, { fallback: BROWSER_FAILED }).detail,
    "Register browser integration in Downpour. The browser could not save it either.",
  );
  assert.equal(rules.handOffNotice(refusal, item, { fallback: NONE }).detail, "Register browser integration in Downpour.");
});

test("a notice is drawn only into the site the download came from", () => {
  assert.equal(rules.sameOrigin("https://host.example/file/1", "https://host.example/other"), true);
  assert.equal(rules.sameOrigin("https://host.example/", "https://bank.example/"), false);
  assert.equal(rules.sameOrigin("https://host.example/", "http://host.example/"), false, "scheme counts");
  assert.equal(rules.sameOrigin("about:blank", "about:blank"), false);
  assert.equal(rules.sameOrigin(undefined, "https://host.example/"), false);
});

test("a stream read again gets its qualities and hides its variants", () => {
  const variant = { kind: "stream", url: "https://cdn.example/v/720.m3u8", heights: [] };
  const master = { kind: "stream", url: "https://cdn.example/v/master.m3u8", playlistKind: "unknown", heights: [], variantUrls: [] };
  const file = { kind: "file", url: "https://cdn.example/clip.mp4" };
  const playlist = { kind: "multivariant", heights: [1080, 720], variantUrls: [variant.url] };
  assert.deepEqual(rules.withPlaylistRead([variant, master, file], master.url, playlist), [
    { ...master, playlistKind: "multivariant", heights: [1080, 720], variantUrls: [variant.url] },
    file,
  ]);
  const media = { kind: "media", heights: [], variantUrls: ["https://cdn.example/v/seg.ts"] };
  assert.deepEqual(rules.withPlaylistRead([master], master.url, media)[0].variantUrls, [], "a media playlist hides nothing");
  assert.equal(rules.withPlaylistRead([file], master.url, playlist), null, "the stream is gone");
  const read = rules.withPlaylistRead([master], master.url, playlist);
  assert.equal(rules.withPlaylistRead(read, master.url, playlist), null, "a second read with the same result changes nothing");
});

test("a refused multivariant playlist is the one read again, not its variants", () => {
  const master = { kind: "stream", url: "https://cdn.example/v/master.m3u8", playlistKind: "unknown", heights: [], frameId: 7 };
  const refusedVariant = { kind: "stream", url: "https://cdn.example/v/720.m3u8", playlistKind: "unknown", heights: [], frameId: 7 };
  const readVariant = { ...refusedVariant, url: "https://cdn.example/v/480.m3u8", playlistKind: "media" };
  assert.equal(rules.streamToReread([readVariant, refusedVariant, master], 7), master);
  assert.equal(rules.streamToReread([readVariant], 7), null, "a media playlist read fine");
  const listed = { ...master, playlistKind: "multivariant", heights: [1080] };
  assert.equal(rules.streamToReread([refusedVariant, listed], 7), null, "the qualities are known");
  assert.equal(rules.needsPlaylistRead({ kind: "stream", url: "https://cdn.example/v/index.m3u8" }), true, "never read");
  assert.equal(rules.needsPlaylistRead({ kind: "file", url: "https://cdn.example/clip.mp4" }), false);
});

test("a source counts as detected only where it was seen loading", () => {
  const entries = [
    { kind: "stream", url: "https://cdn.example/v/master.m3u8", frameId: 7 },
    { kind: "file", url: "https://cdn.example/clip.mp4", frameId: 0 },
  ];
  assert.equal(rules.isDetectedInFrame(entries, entries[0], 7), true);
  assert.equal(rules.isDetectedInFrame(entries, entries[0], 0), false, "the same address in another frame");
  assert.equal(rules.isDetectedInFrame(entries, entries[1], 0), true, "a file");
  assert.equal(rules.isDetectedInFrame(entries, { kind: "file", url: "https://cdn.example/other.mp4" }, 0), false, "only guessed from the video's address");
  assert.equal(rules.isDetectedInFrame(entries, null, 0), false);
});

test("a frame without its own address refers to the page", () => {
  assert.equal(rules.frameReferrer("https://player.example/embed/1", "https://site.example/"), "https://player.example/embed/1");
  assert.equal(rules.frameReferrer("about:blank", "https://site.example/"), "https://site.example/");
  assert.equal(rules.frameReferrer(undefined, "https://site.example/"), "https://site.example/");
});

test("our playlist read sends the page's Referer and Origin", () => {
  const ours = [
    { name: "Accept", value: "*/*" },
    { name: "Origin", value: "moz-extension://abc" },
  ];
  assert.deepEqual(rules.withPageHeaders(ours, { sentReferer: "https://player.example/embed/1", origin: "https://player.example" }), [
    { name: "Accept", value: "*/*" },
    { name: "Referer", value: "https://player.example/embed/1" },
    { name: "Origin", value: "https://player.example" },
  ]);
  assert.deepEqual(rules.withPageHeaders(ours, { sentReferer: "https://player.example/" }), [
    { name: "Accept", value: "*/*" },
    { name: "Origin", value: "moz-extension://abc" },
    { name: "Referer", value: "https://player.example/" },
  ], "a header the page did not send stays ours");
  const odd = [{ name: "Constructor", value: "1" }, { name: "__proto__", value: "2" }];
  assert.deepEqual(rules.withPageHeaders(odd, {}), odd, "any header name is kept");
});

test("a refused playlist is read again a few times, spaced out, per tab", () => {
  let now = 0;
  const budget = rules.createRereadBudget({ limit: 2, gapMs: 3000, clock: () => now });
  const url = "https://cdn.example/v/master.m3u8";
  assert.equal(budget.take(7, url), true);
  assert.equal(budget.take(7, url), false, "too soon");
  now = 3000;
  assert.equal(budget.take(7, url), true);
  now = 60_000;
  assert.equal(budget.take(7, url), false, "the limit is reached");
  assert.equal(budget.take(8, url), true, "another tab has its own count");
  budget.forget(7);
  assert.equal(budget.take(7, url), true, "a new page starts over");
});

test("an unlisted playlist read once keeps its qualities on later asks", () => {
  const memo = rules.createPlaylistMemo();
  const url = "https://cdn.example/v/index.m3u8";
  const playlist = { kind: "multivariant", heights: [720, 480], variantUrls: [] };
  assert.equal(memo.recall(7, url), undefined);
  memo.remember(7, url, playlist);
  assert.equal(memo.recall(7, url), playlist);
  assert.equal(memo.recall(8, url), undefined, "per tab");
  memo.forget(7);
  assert.equal(memo.recall(7, url), undefined, "a new page starts over");
});

test("a Referer cut to the site gives way to the tab's page on that site", () => {
  const page = "https://host.example/file/abc123/clip.mp4";
  assert.equal(rules.downloadPage("https://host.example/", page), page);
  assert.equal(rules.downloadPage("https://host.example/", "https://other.example/x"), "https://host.example/");
  assert.equal(rules.downloadPage("https://host.example/watch/1", page), "https://host.example/watch/1", "a full Referer wins");
  assert.equal(rules.downloadPage("https://host.example/", undefined), "https://host.example/");
  assert.equal(rules.downloadPage(undefined, page), undefined, "no Referer: the tab may be unrelated");
  assert.equal(rules.downloadPage("https://host.example/", "about:blank"), "https://host.example/");
});

test("addresses of this computer are local", () => {
  for (const url of ["http://localhost:8096/v.mp4", "http://app.localhost/", "http://127.0.0.1/", "http://127.1/", "http://[::1]:8080/", "http://0.0.0.0/"]) {
    assert.equal(rules.isLocalAddress(url), true, url);
  }
  for (const url of ["https://example.com/", "http://192.168.1.5/", "http://localhost.example.com/", undefined, "about:blank"]) {
    assert.equal(rules.isLocalAddress(url), false, String(url));
  }
});

test("videos from this computer stay in the browser unless turned on", () => {
  const local = { url: "http://127.0.0.1:8096/movie.mp4" };
  assert.equal(rules.shouldCapture(local, {}).capture, false);
  assert.equal(rules.shouldCapture(local, { captureLocalVideos: true }).capture, true);
  assert.equal(rules.shouldCapture({ url: "http://localhost/setup.exe" }, {}).capture, true, "only videos");
  assert.equal(rules.shouldCapture({ url: "https://example.com/movie.mp4" }, {}).capture, true);
});

test("a video from, or playing on, this computer is ignored by default", () => {
  assert.equal(rules.isIgnoredLocalVideo("http://localhost/live.m3u8", "https://site.example/", {}), true);
  assert.equal(rules.isIgnoredLocalVideo("https://cdn.example/v.mp4", "http://localhost:3000/", {}), true);
  assert.equal(rules.isIgnoredLocalVideo("https://cdn.example/v.mp4", "https://site.example/", {}), false);
  assert.equal(rules.isIgnoredLocalVideo("http://localhost/v.mp4", "http://localhost/", { captureLocalVideos: true }), false);
});

test("a video is named after its page's title", () => {
  assert.equal(rules.videoTitle("  Rome at night "), "Rome at night");
  assert.equal(rules.videoTitle(""), undefined);
  assert.equal(rules.videoTitle(undefined), undefined);
});

test("a page's title names a linked stream, never a linked file", () => {
  assert.equal(rules.linkTitle("https://site.example/ep1.mp4", "Season 1"), undefined);
  assert.equal(rules.linkTitle("https://cdn.example/v/master.m3u8", " Rome "), "Rome");
  assert.equal(rules.linkTitle("https://cdn.example/v/master.m3u8", ""), undefined);
});

test("a download takes the title of the tab only while it shows the starting site", () => {
  const tab = { url: "https://site.example/watch/3", title: " Rome " };
  assert.equal(rules.startingPageTitle(tab, "https://site.example/"), "Rome");
  assert.equal(rules.startingPageTitle({ url: "about:newtab", title: "New Tab" }, "https://site.example/"), undefined);
  assert.equal(rules.startingPageTitle(undefined, "https://site.example/"), undefined);
});

test("only video responses are videos for the local video setting", () => {
  assert.equal(rules.isVideoMime("video/mp4"), true);
  assert.equal(rules.isVideoMime("audio/mpeg"), false);
  assert.equal(rules.isVideoMime(undefined), false);
});

const box = (width, height, left = 0, top = 0) => ({ width, height, left, top, right: left + width, bottom: top + height });

test("the button sits in a large video, or in the player around a hidden one", () => {
  assert.equal(rules.videoBoxChoice(box(640, 360), []), -1);
  assert.equal(rules.videoBoxChoice(box(160, 90), [box(800, 450)]), null, "a thumbnail gets none");
  assert.equal(rules.videoBoxChoice(box(0, 0), [box(100, 20), box(800, 450)]), 1, "nearest large player");
  assert.equal(rules.videoBoxChoice(box(0, 0), [box(100, 20)]), null);
});

test("the main video is the largest, and the first wins a tie", () => {
  assert.equal(rules.largestBox([box(400, 300), box(800, 450), box(800, 450)]), 1);
  assert.equal(rules.largestBox([]), -1);
  assert.equal(rules.largestBox([box(0, 0)]), -1);
});

test("the button stays inside the visible width", () => {
  const options = { width: 40, visibleWidth: 1000, scroll: { x: 0, y: 100 }, gap: 8 };
  assert.deepEqual(rules.videoButtonPosition(box(640, 360, 100, 50), options), { left: 692, top: 158 });
  // Wider than the window: the right edge is the window's, not the video's.
  assert.deepEqual(rules.videoButtonPosition(box(1600, 900, 0, 0), options).left, 952);
  // Starting near the right edge and cut off there: still inside the window.
  assert.equal(rules.videoButtonPosition(box(620, 360, 980, 0), options).left, 952);
  // Cut off on the left: at the visible part's right corner.
  assert.equal(rules.videoButtonPosition(box(640, 360, -500, 0), options).left, 92);
  // In fullscreen there is no scroll.
  assert.deepEqual(rules.videoButtonPosition(box(1920, 1080), { ...options, visibleWidth: 1920, scroll: { x: 0, y: 0 } }), { left: 1872, top: 8 });
});

test("the button rests on the main video, but hides elsewhere and in fullscreen", () => {
  assert.equal(rules.videoButtonHidesWhenLeft({ menuOpen: false, onMainVideo: true, fullscreen: false }), false);
  assert.equal(rules.videoButtonHidesWhenLeft({ menuOpen: false, onMainVideo: false, fullscreen: false }), true);
  assert.equal(rules.videoButtonHidesWhenLeft({ menuOpen: false, onMainVideo: true, fullscreen: true }), true);
  assert.equal(rules.videoButtonHidesWhenLeft({ menuOpen: true, onMainVideo: false, fullscreen: true }), false, "an open menu keeps it");
  assert.equal(rules.videoButtonReturnsToMain({ leftMainVideo: false, fullscreen: false }), true);
  assert.equal(rules.videoButtonReturnsToMain({ leftMainVideo: true, fullscreen: false }), false);
  assert.equal(rules.videoButtonReturnsToMain({ leftMainVideo: false, fullscreen: true }), false);
});

test("fullscreen hides the button later; leaving it brings the button back to the main video", () => {
  assert.equal(rules.videoButtonOnFullscreen({ fullscreen: true, showing: true, onMainVideo: true }), "hide-later");
  assert.equal(rules.videoButtonOnFullscreen({ fullscreen: false, showing: true, onMainVideo: true }), "stay");
  assert.equal(rules.videoButtonOnFullscreen({ fullscreen: false, showing: false, onMainVideo: false }), "show-main");
  assert.equal(rules.videoButtonOnFullscreen({ fullscreen: false, showing: true, onMainVideo: false }), "nothing");
});
