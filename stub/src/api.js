// The theme JS API, window.silk (docs/theme-api.md): the only contract between a theme and the runtime.
// Injected by the runtime before any theme script runs; main.rs fills in the info object. It wraps Tauri
// so themes never touch it. Tauri's own globals may load after this script, so they are read on use.
(function (info) {
  "use strict";
  const listeners = { progress: new Set(), finish: new Set(), error: new Set() };
  const emit = (name, value) => {
    for (const fn of listeners[name]) {
      try { fn(value); } catch (e) { console.error(e); }
    }
  };
  const fail = (e) => { throw new Error(String(e)); };
  const invoke = (cmd) => window.__TAURI__.core.invoke(cmd).catch(fail);
  const tauriWindow = () => window.__TAURI__.window.getCurrentWindow();
  let listening = null;
  let running = null;

  function start() {
    if (running) return running;
    listening ||= window.__TAURI__.event.listen("silk:progress", ({ payload: p }) => {
      const percent = p.bytesTotal ? (p.bytesDone / p.bytesTotal) * 100 : p.total ? (p.done / p.total) * 100 : 0;
      emit("progress", { ...p, percent });
    });
    running = listening.then(() => invoke("start")).then(
      () => { running = null; emit("finish"); },
      (e) => {
        running = null;
        e.cancelled = e.message === "cancelled";
        emit("error", { message: e.message, cancelled: e.cancelled });
        throw e;
      },
    );
    return running;
  }

  function on(name, fn) {
    if (!listeners[name]) throw new Error(`silk.on: unknown event "${name}"`);
    listeners[name].add(fn);
    return () => listeners[name].delete(fn);
  }

  // A frameless theme's title bar: pressing on [data-silk-drag] (outside its controls) moves the window.
  document.addEventListener("mousedown", (e) => {
    const controls = "button, a, input, select, textarea, label";
    if (e.button === 0 && e.target.closest("[data-silk-drag]") && !e.target.closest(controls)) {
      tauriWindow().startDragging().catch(console.error);
    }
  });

  window.silk = Object.freeze({
    info: Object.freeze(info),
    start,
    cancel: () => invoke("cancel"),
    launch: () => invoke("launch"),
    quit: () => invoke("quit"),
    on,
    window: Object.freeze({ minimize: () => tauriWindow().minimize().catch(fail) }),
  });
})(/*INFO*/);
