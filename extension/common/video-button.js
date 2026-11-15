// Content script: shows a small Download button over videos. A click
// downloads the best quality; the arrow next to it
// lists the other qualities of a stream. Runs in every frame, because
// players are often embedded in iframes.
//
// Players usually cover the <video> with control layers that receive the
// mouse events, so the button follows the pointer position across the whole
// window and checks which video lies under it, instead of relying on hover
// events of the video element.
//
// Once the frame's main video is seen loading a stream or file, the button
// stays on it without a hover, so it is there as soon as the video loads.
// Only videos in the page's own DOM are pinned; a video inside a shadow root
// gets the button on hover only.

(() => {
  // Smaller videos are previews, ads, or thumbnails.
  const HIDE_DELAY_MS = 1500;
  const NOTICE_MS = 2600;
  const EDGE_GAP = 8;
  // Custom players nest their <video> in shadow roots, sometimes in layers.
  const MAX_SHADOW_DEPTH = 4;
  // How far up from a hidden <video> its player's box is looked for.
  const MAX_CONTAINER_DEPTH = 3;
  const TITLE = "Download the best quality with Downpour";

  let enabled = true;
  // The stored settings this button follows; capture-rules.js is loaded
  // before this script and owns what they mean.
  let settings = { ...DownpourRules.DEFAULT_SETTINGS };
  let current = null; // the video the button is shown for
  let pinned = null; // the frame's main video, where the button rests
  let hideTimer = null;
  let host = null;
  let parts = null; // the shadow DOM's elements, once created
  // What the button shows for `current`; the DOM is drawn from these in
  // `render` and never read back. `status` is one of
  //   { kind: "idle" }, { kind: "busy" },
  //   { kind: "notice", outcome: "done" | "error", message }.
  let status = { kind: "idle" };
  let choices = []; // the stream's qualities, best first
  let menuOpen = false;
  let noticeTimer = null;
  // The latest pointer position, checked once per animation frame.
  let pointer = null;
  let checkScheduled = false;

  function follow(stored) {
    settings = { ...settings, ...stored };
    enabled = settings.videoButton !== false && !DownpourRules.isIgnoredLocalVideo(undefined, location.href, settings);
    if (!enabled) turnOff();
  }
  follow({});

  // Until the stored settings arrive, the defaults apply, so the first mouse
  // moves are never missed while storage is read.
  browser.storage.local
    .get({ videoButton: true, captureLocalVideos: DownpourRules.DEFAULT_SETTINGS.captureLocalVideos })
    .then(follow)
    .catch((error) => console.warn("Downpour: could not read the video button settings; keeping the defaults", error));

  browser.storage.onChanged?.addListener((changes) => {
    const changed = {};
    for (const name of ["videoButton", "captureLocalVideos"]) {
      if (name in changes) changed[name] = changes[name].newValue;
    }
    if (Object.keys(changed).length > 0) follow(changed);
  });

  function turnOff() {
    pinned = null;
    hide();
  }

  // Only size counts. Players often keep the <video> transparent under a
  // poster until it plays, and the button should be there already.
  function isLarge(video) {
    return video.isConnected && DownpourRules.isLargeBox(video.getBoundingClientRect());
  }

  /** The elements around `video` that may hold its player, nearest first. */
  function surroundingElements(video) {
    const elements = [];
    let element = video.parentElement;
    while (element && elements.length < MAX_CONTAINER_DEPTH) {
      if (element === document.body || element === document.documentElement) break;
      elements.push(element);
      element = element.parentElement;
    }
    return elements;
  }

  /** The element the button sits in for `video`, or null (see DownpourRules.videoBoxChoice). */
  function boxOf(video) {
    if (!video.isConnected) return null;
    const surrounding = surroundingElements(video);
    const choice = DownpourRules.videoBoxChoice(
      video.getBoundingClientRect(),
      surrounding.map((element) => element.getBoundingClientRect()),
    );
    if (choice === null) return null;
    return choice === -1 ? video : surrounding[choice];
  }

  /** The frame's largest video outside shadow roots, or null. */
  function mainVideo() {
    const boxed = [];
    for (const video of document.getElementsByTagName("video")) {
      const box = boxOf(video);
      if (box) boxed.push({ video, rect: box.getBoundingClientRect() });
    }
    const index = DownpourRules.largestBox(boxed.map((entry) => entry.rect));
    return index === -1 ? null : boxed[index].video;
  }

  function contains(rect, x, y) {
    return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
  }

  /** The large video under the point, looked up fresh (players replace their elements). */
  function videoAt(x, y) {
    for (const video of document.getElementsByTagName("video")) {
      if (contains(video.getBoundingClientRect(), x, y) && isLarge(video)) return video;
    }
    return videoInShadowAt(document, x, y, 0);
  }

  /**
   * A video inside the shadow roots of the elements at the point.
   * `getElementsByTagName` does not see into shadow roots; hit testing finds
   * the few hosts actually under the pointer instead of walking the page.
   */
  function videoInShadowAt(scope, x, y, depth) {
    if (depth >= MAX_SHADOW_DEPTH) return null;
    for (const element of scope.elementsFromPoint(x, y)) {
      // Firefox lets content scripts into closed shadow roots as well.
      const shadow = element.openOrClosedShadowRoot ?? element.shadowRoot;
      if (!shadow || element === host) continue;
      for (const video of shadow.querySelectorAll("video")) {
        if (contains(video.getBoundingClientRect(), x, y) && isLarge(video)) return video;
      }
      const nested = videoInShadowAt(shadow, x, y, depth + 1);
      if (nested) return nested;
    }
    return null;
  }

  const ICON_DOWNLOAD = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 2.5v7.5M4.5 6.8 8 10.3l3.5-3.5M3 13.5h10"/></svg>`;
  const ICON_DONE = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M3.5 8.5 6.5 11.5 12.5 4.5"/></svg>`;
  const ICON_ERROR = `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M8 3.5v5.5M8 12.2v.1"/></svg>`;
  const ICON_BUSY = `<svg class="spin" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M8 2.5a5.5 5.5 0 1 1-5.5 5.5"/></svg>`;
  const ICONS = { idle: ICON_DOWNLOAD, busy: ICON_BUSY, done: ICON_DONE, error: ICON_ERROR };

  function ensureButton() {
    if (host) return;
    host = document.createElement("downpour-video-button");
    const shadow = host.attachShadow({ mode: "open" });
    shadow.innerHTML = `
      <style>
        :host { all: initial; position: fixed; z-index: 2147483647; top: 0; left: 0; display: none; }
        .bar {
          display: inline-flex; align-items: stretch; height: 26px; border-radius: 13px; overflow: hidden;
          font: 600 11.5px/1 "Segoe UI", system-ui, sans-serif; color: #fff; white-space: nowrap;
          background: rgba(61, 65, 72, 0.88); box-shadow: 0 4px 12px rgba(0, 0, 0, 0.35);
          backdrop-filter: blur(6px);
        }
        button {
          all: initial; box-sizing: border-box; display: inline-flex; align-items: center; gap: 5px;
          cursor: pointer; color: inherit; font: inherit; transition: background 120ms ease;
        }
        button:hover, button[aria-expanded="true"] { background: #f8615a; }
        .main { padding: 0 8px; }
        .label:empty { display: none; }
        .more { padding: 0 6px 0 5px; border-left: 1px solid rgba(255, 255, 255, 0.18); }
        .more[hidden] { display: none; }
        .bar[data-state="busy"] .main { cursor: progress; }
        .bar[data-state="done"] { background: #f8615a; }
        .bar[data-state="error"] { background: #f6e04b; color: #2f3238; }
        svg { width: 14px; height: 14px; flex: none; display: block; }
        .more svg { width: 10px; height: 10px; }
        .spin { animation: spin 0.8s linear infinite; }
        @keyframes spin { to { transform: rotate(360deg); } }
        .menu, .notice {
          position: absolute; top: 30px; right: 0; min-width: 112px; padding: 4px;
          border-radius: 10px; background: rgba(64, 68, 75, 0.97); box-shadow: 0 8px 22px rgba(0, 0, 0, 0.4);
          font: 500 12px/1.2 "Segoe UI", system-ui, sans-serif; color: #fff;
        }
        .menu[hidden], .notice[hidden] { display: none; }
        .menu button { display: flex; width: 100%; padding: 6px 10px; border-radius: 999px; }
        .notice { width: max-content; max-width: 240px; padding: 7px 10px; white-space: normal; }
      </style>
      <div class="bar" data-state="idle">
        <button class="main" type="button" title="${TITLE}"><span class="icon"></span><span class="label"></span></button>
        <button class="more" type="button" title="Choose the quality" aria-haspopup="menu" aria-expanded="false" hidden>
          <svg viewBox="0 0 10 10" fill="currentColor"><path d="M1.5 3.5h7L5 7.5z"/></svg>
        </button>
      </div>
      <div class="menu" role="menu" hidden></div>
      <div class="notice" role="status" hidden></div>`;
    parts = {
      bar: shadow.querySelector(".bar"),
      main: shadow.querySelector(".main"),
      icon: shadow.querySelector(".icon"),
      label: shadow.querySelector(".label"),
      more: shadow.querySelector(".more"),
      menu: shadow.querySelector(".menu"),
      notice: shadow.querySelector(".notice"),
    };
    parts.main.addEventListener("click", (event) => {
      stop(event);
      download(undefined); // the background reads no quality as the best
    });
    parts.more.addEventListener("click", (event) => {
      stop(event);
      if (menuOpen) closeMenu();
      else openMenu();
    });
    host.addEventListener("mouseenter", () => {
      clearTimeout(hideTimer);
      refreshChoices();
    });
    host.addEventListener("mouseleave", scheduleHide);
    render();
  }

  // Players treat clicks that reach them as play/pause.
  function stop(event) {
    event.preventDefault();
    event.stopPropagation();
  }

  /** Puts the button at the video's top-right corner, inside the viewport. */
  function place() {
    if (!current || !host) return;
    const box = boxOf(current);
    if (!box) {
      if (current === pinned) pinned = null;
      leaveVideo();
      return;
    }
    // In fullscreen only the fullscreen element is drawn; move inside it.
    const container = document.fullscreenElement ?? document.documentElement;
    if (host.parentNode !== container) container.append(host);
    const rect = box.getBoundingClientRect();
    const width = parts.bar.offsetWidth || 40;
    // The button sits at the video's top-right corner and scrolls with it,
    // so it never covers the page outside the video. The fullscreen element
    // is fixed to the screen; the page is positioned from its top-left.
    const fullscreen = container === document.fullscreenElement;
    host.style.position = fullscreen ? "fixed" : "absolute";
    const { left, top } = DownpourRules.videoButtonPosition(rect, {
      width,
      visibleWidth: fullscreen ? window.innerWidth : document.documentElement.clientWidth,
      scroll: fullscreen ? { x: 0, y: 0 } : { x: window.scrollX, y: window.scrollY },
      gap: EDGE_GAP,
    });
    host.style.transform = `translate(${left}px, ${top}px)`;
  }

  function show(video) {
    if (!enabled) return;
    // A page elsewhere can still play a video from this computer.
    if (DownpourRules.isIgnoredLocalVideo(sourceOf(video), location.href, settings)) return;
    ensureButton();
    clearTimeout(hideTimer);
    if (current !== video) {
      current = video;
      clearTimeout(noticeTimer);
      status = { kind: "idle" };
      choices = [];
      menuOpen = false;
      refreshChoices();
    }
    host.style.display = "block";
    render();
  }

  function scheduleHide() {
    clearTimeout(hideTimer);
    const hides = DownpourRules.videoButtonHidesWhenLeft({
      menuOpen,
      onMainVideo: current === pinned,
      fullscreen: Boolean(document.fullscreenElement),
    });
    if (hides) hideTimer = setTimeout(leaveVideo, HIDE_DELAY_MS);
  }

  function hide() {
    clearTimeout(hideTimer);
    menuOpen = false;
    if (host) host.style.display = "none";
    current = null;
  }

  /** The pointer left a video: hides the button, which goes back to the main video if it left that one. */
  function leaveVideo() {
    const left = current;
    hide();
    const returns = DownpourRules.videoButtonReturnsToMain({
      leftMainVideo: left === pinned,
      fullscreen: Boolean(document.fullscreenElement),
    });
    if (returns) showPinnedVideo();
  }

  /** Shows the button on the main video; forgets a main video that is gone. */
  function showPinnedVideo() {
    if (!pinned) return;
    if (!boxOf(pinned)) {
      pinned = null;
      return;
    }
    show(pinned);
  }

  /**
   * Keeps the button on the frame's main video once the add-on saw that
   * video's stream or file load in this frame.
   */
  async function pinMainVideo() {
    if (!enabled) return;
    const video = mainVideo();
    if (!video || video === pinned) return;
    if (DownpourRules.isIgnoredLocalVideo(sourceOf(video), location.href, settings)) return;
    const reply = await askQualities(video);
    if (!reply?.detected || !enabled || video !== mainVideo()) return;
    const previous = pinned;
    pinned = video;
    // The player may have replaced the element the button was resting on.
    if (!current || current === previous || !current.isConnected) show(video);
  }

  /**
   * The height the button shows: the stream's best quality, or the height
   * the video plays at when its qualities are not known (a single file, or
   * a stream with one quality).
   */
  function heightToShow() {
    const best = choices[0]?.height;
    if (best) return best;
    return current?.videoHeight > 0 ? current.videoHeight : undefined;
  }

  /** Draws the button from `status`, `choices`, and `menuOpen`. */
  function render() {
    if (!parts) return;
    const shownHeight = heightToShow();
    const look = status.kind === "notice" ? status.outcome : status.kind;
    parts.bar.dataset.state = look;
    parts.icon.innerHTML = ICONS[look];
    parts.main.title = look === "done" ? "Sent to Downpour" : TITLE;
    parts.label.textContent = shownHeight ? `${shownHeight}p` : "";
    parts.more.hidden = choices.length < 2;
    parts.more.setAttribute("aria-expanded", String(menuOpen));
    parts.menu.hidden = !menuOpen;
    const notice = status.kind === "notice" && status.outcome === "error" ? status.message : "";
    parts.notice.textContent = notice;
    parts.notice.hidden = !notice || menuOpen;
    place();
  }

  /**
   * Asks which qualities the video's stream has. A stream is often found
   * only after the player loads it, so this runs again when the pointer
   * comes back to the button.
   */
  async function refreshChoices() {
    const video = current;
    if (!video) return;
    const reply = await askQualities(video);
    if (video !== current || !Array.isArray(reply?.choices)) return;
    choices = reply.choices;
    if (choices.length < 2) menuOpen = false;
    render();
  }

  /** What the add-on knows about the video's stream, or null when it could not ask. */
  async function askQualities(video) {
    try {
      return await browser.runtime.sendMessage({ type: "video-qualities", src: sourceOf(video) });
    } catch (error) {
      // The button still downloads the best quality, just without a menu.
      console.warn("Downpour: could not list the video's qualities", error);
      return null;
    }
  }

  function openMenu() {
    if (!parts || choices.length < 2) return;
    parts.menu.replaceChildren(
      ...choices.map((choice) => {
        const item = document.createElement("button");
        item.type = "button";
        item.setAttribute("role", "menuitem");
        item.textContent = choice.label;
        item.addEventListener("click", (event) => {
          stop(event);
          closeMenu();
          download(choice.quality);
        });
        return item;
      }),
    );
    menuOpen = true;
    clearTimeout(hideTimer);
    render();
  }

  function closeMenu() {
    if (!menuOpen) return;
    menuOpen = false;
    render();
  }

  function sourceOf(video) {
    return video.currentSrc || video.src;
  }

  /** Sends the video to the app; `quality` undefined means the best. */
  async function download(quality) {
    const video = current;
    if (!video || status.kind === "busy") return;
    clearTimeout(noticeTimer);
    status = { kind: "busy" };
    render();
    let reply;
    try {
      reply = await browser.runtime.sendMessage({ type: "download-video", src: sourceOf(video), quality });
    } catch (error) {
      reply = { type: "error", message: String(error?.message ?? error) };
    }
    // The pointer moved on to another video meanwhile; its button is not ours.
    if (video !== current) return;
    status = reply?.type === "accepted"
      ? { kind: "notice", outcome: "done", message: "" }
      : { kind: "notice", outcome: "error", message: reply?.message ?? "Downpour did not answer." };
    render();
    noticeTimer = setTimeout(() => {
      status = { kind: "idle" };
      render();
    }, NOTICE_MS);
  }

  function onPointerMove(event) {
    if (!enabled) return;
    if (host && event.composedPath?.().includes(host)) {
      clearTimeout(hideTimer);
      return;
    }
    pointer = { x: event.clientX, y: event.clientY };
    if (!checkScheduled) {
      checkScheduled = true;
      requestAnimationFrame(checkPointer);
    }
  }

  function checkPointer() {
    checkScheduled = false;
    if (!pointer) return;
    const video = videoAt(pointer.x, pointer.y);
    if (video) {
      show(video);
      // In fullscreen the pointer never leaves the video; hide when it rests.
      if (document.fullscreenElement) scheduleHide();
    } else if (current) {
      scheduleHide();
    }
  }

  // A click anywhere else closes the quality menu.
  function onPointerDown(event) {
    if (menuOpen && !event.composedPath?.().includes(host)) {
      closeMenu();
      scheduleHide();
    }
  }

  function onKeyDown(event) {
    if (menuOpen && event.key === "Escape") closeMenu();
  }

  // A video starting to play (e.g. autoplay) shows the button without hover.
  // Media events do not leave shadow roots, so this sees light-DOM videos.
  function onPlay(event) {
    if (enabled && event.target instanceof HTMLVideoElement && isLarge(event.target)) {
      show(event.target);
      scheduleHide();
    }
  }

  function onVideoResize(event) {
    if (event.target === current) render();
  }

  // A video that loads under a pointer that is not moving would otherwise
  // wait for the next mouse move; check the last known position again.
  function onVideoReady(event) {
    if (!enabled || !(event.target instanceof HTMLVideoElement)) return;
    pinMainVideo();
    if (pointer && !checkScheduled) {
      checkScheduled = true;
      requestAnimationFrame(checkPointer);
    }
  }

  function onFullscreenChange() {
    place();
    const action = DownpourRules.videoButtonOnFullscreen({
      fullscreen: Boolean(document.fullscreenElement),
      showing: Boolean(current),
      onMainVideo: current !== null && current === pinned,
    });
    if (action === "hide-later") scheduleHide();
    // Back on the page, the button stays on the main video again.
    else if (action === "stay") clearTimeout(hideTimer);
    else if (action === "show-main") showPinnedVideo();
  }

  // The background found new media in the tab: maybe this frame's stream,
  // or the qualities of the one the button shows.
  function onBackgroundMessage(message) {
    if (message?.type !== "media-changed" || !enabled) return;
    if (current) refreshChoices();
    pinMainVideo();
  }

  // The manifest runs this script at document_start, so these listeners
  // exist before the page's own scripts and ads finish loading; nothing
  // touches the page until a video is found.
  window.addEventListener("mousemove", onPointerMove, { capture: true, passive: true });
  window.addEventListener("pointerdown", onPointerDown, { capture: true, passive: true });
  window.addEventListener("keydown", onKeyDown, { capture: true });
  document.addEventListener("play", onPlay, { capture: true });
  document.addEventListener("loadedmetadata", onVideoReady, { capture: true });
  // A video's height is known once it loads and changes when the player
  // switches quality; the label follows it.
  document.addEventListener("resize", onVideoResize, { capture: true });
  document.addEventListener("playing", onVideoReady, { capture: true });
  window.addEventListener("scroll", place, { capture: true, passive: true });
  window.addEventListener("resize", place, { passive: true });
  document.addEventListener("fullscreenchange", onFullscreenChange);
  browser.runtime.onMessage.addListener(onBackgroundMessage);
})();
