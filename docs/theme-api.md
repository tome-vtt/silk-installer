# Writing a Silk Setup theme

A theme is a folder of plain HTML, CSS, scripts, images and fonts. The installer shows it in its
window and gives it one global object, `silk`, which is all a theme needs to talk to the installer.
No build step, no framework, no Tauri.

## Folder layout

```
my-theme/
  index.html        required: the page the window opens
  style.css         anything else is up to you, loaded by relative path
  img/hero.webp
  fonts/Inter.woff2
```

Name the folder in the config with `theme = "my-theme"` (relative to the config file). `silk build`
packs the whole folder into the installer and refuses a folder without `index.html`. Without a `theme`
line the installer uses the built-in default theme ([stub/ui/index.html](../stub/ui/index.html), a
good starting point to copy).

The page is served from `http://silk.localhost/`, so relative paths such as `img/hero.webp` or
`url("../fonts/Inter.woff2")` work. Paths are case-sensitive, unlike on disk. Links to the web do not
open inside the installer.

## Testing a theme

Build an installer with your theme and run it. It installs for real, so use a test product name and
uninstall it afterwards from Settings > Apps > Installed apps.

```powershell
.\target\release\silk.exe build my-app.toml
# To inspect the page, open edge://inspect in Edge while the installer runs with:
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9222"
.\my-app-1.0.0-setup.exe
```

`silk` exists only inside the installer; opening `index.html` in a browser shows the page without it.

## The API

`silk` is ready before your first script runs, so you can read `silk.info` straight away.

### `silk.info`

Fixed for the life of the window.

| Field | |
|---|---|
| `product`, `version`, `publisher` | strings from the config |
| `target` | the install folder, e.g. `C:\Users\Ann\AppData\Local\Programs\Sample` |
| `installed` | the version already installed there, or `null` |
| `alreadyCurrent` | `true` when `installed` is this version or newer (offer "Reinstall") |
| `launchAtFinish` | the config's `launch_at_finish`: whether to offer launching the app |
| `files`, `size` | number of files to install and their total size in bytes |
| `frame` | `false` when the window has no native title bar (see below) |

### Calls

| Call | |
|---|---|
| `silk.start()` | Install. Returns a promise that resolves when done and rejects with an `Error` (its `cancelled` is `true` after `cancel()`). Calling it again while it runs returns the same promise; after a failure it can be called again. |
| `silk.cancel()` | Stop a running install while files are copied; nothing is left behind. No effect once the phase is `finishing`. |
| `silk.launch()` | Start the installed app. Returns a promise; rejects with an `Error`. |
| `silk.quit()` | Close the installer. |
| `silk.on(event, fn)` | Listen for an event (below). Returns a function that stops listening. An unknown event name throws. |
| `silk.window.minimize()` | Minimise the window, for a theme's own title bar. |

### Events

| Event | `fn` receives |
|---|---|
| `progress` | `{ phase, file, done, total, bytesDone, bytesTotal, percent }` |
| `finish` | nothing: the install is done |
| `error` | `{ message, cancelled }`: `message` is written for the user |

`phase` is `"preparing"` (checking space), `"copying"` (writing files; one event per file, `file`
is the path just written, e.g. `resources/app.asar`), then `"finishing"` (closing the running app,
moving the files into place, shortcuts, Installed Apps). More phases may be added: show unknown ones
as they come or ignore them. `done`/`total` count files and `bytesDone`/`bytesTotal` count bytes;
`percent` (0 to 100, not rounded) follows the bytes, so it also gives a time-remaining estimate:
`elapsed * (bytesTotal - bytesDone) / bytesDone`.

## Frameless windows

In the config, `[window]` takes `frame = false` to drop the native title bar and border, so the
theme draws its own. Then:

- mark the title bar (or any area) with the `data-silk-drag` attribute: pressing the mouse there moves
  the window; buttons, links and form fields inside it still work;
- a minimise button calls `silk.window.minimize()`, a close button `silk.quit()`. Closing mid-install
  ends the process, so call `silk.cancel()` first and quit on the `error` event, as the default theme
  does.

With the native frame, `minimize = false` and `close = false` hide or disable those native buttons.
The window never resizes or maximises.

## A minimal theme

[examples/themes/minimal/index.html](../examples/themes/minimal/index.html), used by
[examples/minimal-theme.toml](../examples/minimal-theme.toml):

```html
<!doctype html>
<meta charset="utf-8">
<body style="font: 15px system-ui; padding: 24px">
<h2 id="title"></h2>
<p id="status"></p>
<button id="go">Install</button>
<button id="launch" hidden>Launch</button>
<button onclick="silk.quit()">Close</button>
<script>
  const { product, version, installed } = silk.info;
  const $ = (id) => document.getElementById(id);
  $("title").textContent = `${product} ${version}`;
  $("status").textContent = installed ? `Version ${installed} is installed.` : "Not installed yet.";
  silk.on("progress", (p) => { $("status").textContent = `${p.phase} ${Math.floor(p.percent)}% ${p.file}`; });
  silk.on("finish", () => { $("status").textContent = "Done."; $("launch").hidden = false; });
  silk.on("error", (e) => { $("status").textContent = e.cancelled ? "Cancelled." : `Failed: ${e.message}`; $("go").hidden = false; });
  $("go").onclick = () => { $("go").hidden = true; silk.start().catch(() => {}); };
  $("launch").onclick = () => silk.launch().then(silk.quit);
</script>
```
