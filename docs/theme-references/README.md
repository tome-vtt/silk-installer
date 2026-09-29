# Theme references

These images were supplied by Bart on 2026-09-29, in two sets of four, as references for what Silk
Setup themes must be able to do. They are not designs to copy. The product in them, "LumaForge", is a made-up example,
and the look of each screen is up to whoever writes a theme. What matters is the behaviour each
screen needs from the runtime and from the theme JS API (`docs/spec.md` § 4), which is what the
notes below describe.

## installing-dark-sidebar.webp

An install-progress screen in a dark theme. A translucent, glass-like panel sits over a full-bleed
background that looks animated (flowing particles), so the theme must be able to fill the whole
window with its own artwork and effects rather than living inside a fixed frame.

A sidebar lists the steps Prepare, Download, Extract and Install, each with a short status line and
a done or current state. To drive that, the theme needs to know which phase the install is in, not
just an overall percent. The main area shows a large percent figure, a progress bar, the elapsed
time and an estimated time remaining; a theme can time the elapsed part itself, but a good estimate
of what remains needs the runtime to report bytes done against bytes total. A collapsible "Show
details" section implies a per-file log, so the runtime must report each file as it is written. The
Finish button stays disabled until the install is done, and the panel carries the author's own brand
text and tagline.

## complete-light.webp

A finish screen in a light theme, with the same step list in a sidebar, all steps now done. The main
action is a "Launch LumaForge" button, which the existing `launch()` call covers. Below it, a "View
quick start" link asks the runtime to open a URL or a local file (such as a bundled guide) in the
user's default browser. A "Create desktop shortcut" checkbox lets the user decide, on this screen,
whether a desktop shortcut is made, so a user's choice has to be able to change what the actions do,
here even after the main copy has finished.

## welcome-frameless.webp

A welcome screen in a frameless window. The theme draws its own minimise, maximise and close buttons
and must be able to mark an area of the page as the drag handle for moving the window. A numbered
stepper across the top shows Welcome, Options, Install and Complete, and a large hero image fills
the right half.

The install location is an editable field with a Browse button, so the user can type a target
directory or pick one through the native Windows folder picker, and the chosen folder must then be
used by the install. The default path shown is under `C:\Program Files`, which is a per-machine
install.

## components-dark.webp

A "Choose components" screen in a dark theme. The user ticks optional components (core app, asset
library, plugins, sample projects), each showing its size. An installation summary lists what is
selected, adds up the estimated disk space and compares it with the free space available on the
target drive. The install folder is shown with a Change button (the same folder picker as above),
and Back and Install buttons move between pages.

## Second set: compact window (2026-09-29)

A second set of four, all in a small, compact window with a custom title bar: the product's icon and
name on the left, and minimise, maximise and close buttons drawn by the theme on the right. A row of
four dots or a short stepper shows which page the user is on.

### compact-welcome-light.webp

A welcome screen in a light theme: the app logo, "Install LumaForge", a one-line tagline, and an
install location field with a Browse… button (the same folder picker as the first set), defaulting
to `C:\Program Files\LumaForge`, a per-machine path. A Continue button moves to the next page. The
title bar shows the product icon and name, which the theme can take from the config rather than
carrying its own copies.

### compact-components-dark.webp

A "Choose what to install" screen in a dark theme with two components, Core app and Asset library,
each a checkbox with a one-line description. Unlike the first set, the sizes are not shown per
component: a single line reads "Requires 2.8 GB of disk space", the total for what is ticked right
now, so it must update as the user ticks and unticks. Back and Install buttons move between pages.

### compact-installing-dark.webp

An install-progress screen in a dark theme. Above the progress bar is a label for the current phase,
"Copying files", which the theme needs to change as the install moves on to other phases. The bar
has a percent beside it, then "About 1 minute remaining", then a collapsible Details section (the
per-file log from the first set).

The title bar here has only minimise and close, with no maximise. So which window buttons appear is
up to the theme or the config, and can differ from page to page in the same installer. Close is still
shown during the install, so the runtime must not simply kill the process mid-copy when the window is
closed: the theme needs to hear about it, and either ignore it, ask the user to confirm, or call
`cancel()` and let the rollback run.

### compact-complete-light.webp

A finish screen in a light theme. The stepper shows all four steps ticked, a large tick sits over
"LumaForge is ready" and "The installation completed successfully.", with a Launch app button and a
plain Close link under it. Both are covered by `launch()` and `quit()`.

## Capabilities these imply

Items marked **not in spec** are not covered by `docs/spec.md` §§ 4–5 as written on 2026-09-29.

- **User-chosen install folder with a native folder picker** (not in spec). The config declares the
  target directory and themes can read it, but there is no call to change it or to open a folder
  picker.
- **Free-space and payload-size information** (not in spec). § 5 has a free-space check inside the
  copy action, but themes are not told the payload size or the free space on the target drive.
- **Elapsed and remaining time, or the data to compute it** (partly in spec). Progress events carry
  the current file and a percent, from which a theme can time elapsed itself and make a rough
  estimate of what remains. Bytes done and bytes total, and the current phase for a step list, are
  not in spec.
- **Per-file detail events**. Covered: progress events include the current file, which is enough for
  a details log.
- **User options that change actions, such as the desktop shortcut** (not in spec). § 5 makes the
  desktop shortcut optional in config, chosen by the author, not by the user at run time.
- **Optional components** (not in spec). The payload is a single directory, with no way to split it
  into parts the user can choose.
- **Open a URL or file in the default browser** (not in spec). The API has `launch()` for the main
  exe only.
- **Frameless window with theme-drawn window controls and dragging** (not in spec). § 4 sets only the
  window size in config; there are no calls to minimise, maximise, close the window or mark a drag
  area.
- **Choosing which window buttons appear, and whether the window can be resized** (not in spec). The
  compact set shows maximise on some pages and not on the install page. A theme drawing its own
  buttons can leave one out, but the runtime also has to allow or block resizing to match.
- **Handling close while the install runs** (not in spec). Closing the window (the close button,
  Alt+F4, or the taskbar) must reach the theme as an event it can refuse, confirm or turn into
  `cancel()`, rather than ending the process part-way through a copy.
- **A label for the current phase** (not in spec). A line such as "Copying files" needs the runtime
  to say which phase it is in (copying, shortcuts, registering, running a program), as a fixed name
  the theme turns into its own wording. This is the same phase report the step list needs above.
- **Disk space for the current selection** (not in spec). Follows from optional components: if each
  component's size is known, the theme can add up what is ticked and update the total as it changes.
- **The product icon for the title bar** (not in spec). The config declares an icon, but the API
  offers themes only the product name, version, target and installed version. A theme can carry its
  own copy of the icon in the meantime.
- **Per-machine installs**. Covered: § 2 allows per-machine scope (UAC, `Program Files`, HKLM), and
  § 5 writes the Installed Apps entry to HKLM for it. All-users shortcuts are still an open question
  in § 11.
- **Optional online download of files** (deferred). Planned as a later, optional expansion; for now
  every installer carries all its files inside the exe.

The "Download" step in the first two images would need the installer to fetch its files online at
install time. Bart decided on 2026-09-29 that this is an optional expansion planned for later. For
now an installer carries everything inside the exe, so a theme could show a Download step, but Silk
Setup would have nothing to report for it yet.
