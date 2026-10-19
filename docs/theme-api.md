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

## Install and uninstall

One theme serves both. `silk build` packs an uninstaller carrying the same theme, which the installer
puts in the install folder as `Uninstall <product>.exe`. Installed Apps runs it with `--uninstall`, which opens the theme with
`silk.info.mode` set to `"uninstall"`. The same calls then mean "uninstall": `start()` removes the app,
progress events report it and `finish` ends it. Show a confirm screen first ("Remove Sample?"), a
progress screen, then a done screen with a Close button. After a successful uninstall the uninstaller
deletes itself once the window closes; closing it without starting leaves everything as it was.
`--uninstall --silent` uninstalls without a window, for IT tools.

To test it, install your build, then run the uninstaller from Settings > Apps > Installed apps, or
directly: `& "$env:LOCALAPPDATA\Programs\My App\Uninstall My App.exe" --uninstall`.

## The API

`silk` is ready before your first script runs, so you can read `silk.info` straight away.

### `silk.info`

Fixed for the life of the window.

| Field | |
|---|---|
| `mode` | `"install"`, or `"uninstall"` when Installed Apps runs the uninstaller |
| `product`, `version`, `publisher` | strings from the config |
| `scope` | `"user"` (installs for the current user only) or `"machine"` (for all users of the PC; the installer already has administrator rights when the window opens) |
| `target` | the install folder, e.g. `C:\Users\Ann\AppData\Local\Programs\Sample` or `C:\Program Files\Sample` |
| `installed` | the version already installed there, or `null` |
| `alreadyCurrent` | `true` when `installed` is this version or newer (offer "Reinstall") |
| `launchAtFinish` | whether to offer launching the app: the config's `launch_at_finish`, but `false` for a per-machine installer that was started with administrator rights (it will not start the app as administrator) |
| `resumed` | `true` when `silk.reboot()` restarted Windows and this is the run that came back: it starts by itself (below) |
| `files`, `size` | number of files to install and their total size in bytes; `0` when uninstalling |
| `frame` | `false` when the window has no native title bar (see below) |

### Calls

| Call | |
|---|---|
| `silk.start()` | Install, or uninstall in uninstall mode. Returns a promise that resolves when done and rejects with an `Error` (its `cancelled` is `true` after `cancel()`). Calling it again while it runs returns the same promise; after a failure it can be called again. |
| `silk.cancel()` | Stop a running install while files are copied or a `[[run]]` program runs, which it ends along with what it started; a process the program deliberately starts outside its job (`CREATE_BREAKAWAY_FROM_JOB`) keeps running. It rolls back only Silk's own files: what a before-install program that already succeeded did stays (see `docs/spec.md` §3). Once the phase is `finishing` it stops only an after-install program, and it has no effect on an uninstall, which cannot be stopped once started: the confirm screen is the way out. |
| `silk.launch()` | Start the installed app (install mode only). Returns a promise; rejects with an `Error`, always so for a per-machine installer started with administrator rights (see `launchAtFinish`). A per-machine installer starts the app once its window closes, so follow it with `silk.quit()`. |
| `silk.write(line)` | Write one line to the stdin of the `talk` program running now (below). Returns a promise; rejects with an `Error` when none is running, or `line` holds a line break. |
| `silk.openUrl(url)` | Open an `http://` or `https://` address in the user's default browser. Returns a promise; rejects with an `Error` for any other address, or one holding a `"`. |
| `silk.reboot()` | Restart Windows and start this installer again after sign-in (below). Returns a promise; rejects with an `Error` while `start()` runs, in uninstall mode, or when Windows could not be restarted. |
| `silk.quit()` | Close the installer. |
| `silk.on(event, fn)` | Listen for an event (below). Returns a function that stops listening. An unknown event name throws. |
| `silk.window.minimize()` | Minimise the window, for a theme's own title bar. |

### Events

| Event | `fn` receives |
|---|---|
| `progress` | `{ phase, file, done, total, bytesDone, bytesTotal, percent }` |
| `finish` | nothing: the install or uninstall is done |
| `error` | `{ message, cancelled }`: `message` is written for the user |
| `output` | a string: one line a `talk` program printed, without its line break |
| `exit` | a number: a `talk` program's exit code, once it has ended; none for one `cancel()` ended |

`phase` is `"preparing"` (checking space; with before-install programs, also closing the running app
and running them), `"copying"` (writing files; one event per file, `file`
is the path just written, e.g. `resources/app.asar`), then `"finishing"` (closing the running app,
moving the files into place, shortcuts, Installed Apps). More phases may be added: show unknown ones
as they come or ignore them. An uninstall reports `"preparing"` (running the config's before-uninstall
programs, closing the app), `"removing"` (deleting files; one event per file, `file` is the path just
removed) and `"finishing"` (shortcuts, registry values, the Installed Apps entry). `done`/`total` count files and `bytesDone`/`bytesTotal` count bytes;
`percent` (0 to 100, not rounded) follows the bytes, so it also gives a time-remaining estimate:
`elapsed * (bytesTotal - bytesDone) / bytesDone`.

## Talking to a program

A `[[run]]` program with `talk = true` in the config uses the theme as its face, for a setup program
that asks questions. While `start()` runs it, each line it prints on stdout comes to the theme as an
`output` event, in order, and its exit code as an `exit` event when it ends; `silk.write(line)` sends
it a line on stdin (Windows line ending added). Silk only moves lines: what they say is up to the
program and the theme. Programs run one at a time, in the config's order, so `output` always comes from
the one running now. A program waiting on stdin waits until the theme writes, so answer every
question it asks, or `silk.cancel()` to end it and whatever it started (except a process it starts with `CREATE_BREAKAWAY_FROM_JOB`, which keeps running); once it ends, its stdin
closes. It counts as ended when its stdout closes, so a
program it starts in the background must not keep that open. Lines go both ways as UTF-8: a `cmd`
script runs `chcp 65001` first so non-English text arrives intact. A nonzero exit still fails the install unless
`fail_on_nonzero = false`. A per-machine installer runs it with administrator rights.

```js
silk.on("output", (line) => {
  if (line.startsWith("open ")) silk.openUrl(line.slice(5));
  else if (line === "name?") silk.write(document.getElementById("name").value);
  else document.getElementById("log").append(line + "\n");
});
silk.on("exit", (code) => console.log("setup program exited with", code));
```

## Restarting Windows

A setup program that needs a restart to carry on (enabling a Windows feature, say) tells the theme,
for instance with a line of output, and ends. The theme asks the user, its own way: a "Restart now"
button, or a countdown with "Not yet". Calling `silk.reboot()` is the user's yes, so Silk asks
nothing more. It registers the installer to start once at the next sign-in, with the same arguments
and rights, then restarts Windows. "Not yet" simply never calls it: nothing is registered.

After sign-in the installer opens again with `silk.info.resumed` set, and once the page has loaded it
starts the install by itself, setup programs included, as if `silk.start()` had been called: show the
progress page straight away, and `silk.start()` gives the same promise. It starts once per window:
once the install has started, a page the theme opens later does not start it again. The setup
program must skip what it already did. The registration is used once: a later sign-in does not start the installer.

```js
silk.on("output", (line) => { if (line === "reboot-required") show("restart"); });
document.getElementById("restart-now").onclick = () => silk.reboot().catch((e) => alert(e.message));
if (silk.info.resumed) show("progress");
```

The install runs again in full: the files are copied again before the setup programs run. A
per-machine installer asks for administrator rights again when it comes back, as it did the first
time. An uninstall cannot restart Windows: `silk.reboot()` rejects in uninstall mode. To try it, build a test installer and run it on a spare PC: it really
restarts Windows.

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
  const { mode, product, version, installed } = silk.info;
  const $ = (id) => document.getElementById(id);
  const removing = mode === "uninstall";
  $("title").textContent = removing ? `Remove ${product}?` : `${product} ${version}`;
  $("status").textContent = installed ? `Version ${installed} is installed.` : "Not installed yet.";
  if (removing) $("go").textContent = "Remove";
  silk.on("progress", (p) => { $("status").textContent = `${p.phase} ${Math.floor(p.percent)}% ${p.file}`; });
  silk.on("finish", () => { $("status").textContent = removing ? "Removed." : "Done."; $("launch").hidden = removing; });
  silk.on("error", (e) => { $("status").textContent = e.cancelled ? "Cancelled." : `Failed: ${e.message}`; $("go").hidden = false; });
  $("go").onclick = () => { $("go").hidden = true; silk.start().catch(() => {}); };
  $("launch").onclick = () => silk.launch().then(silk.quit);
</script>
```
