# Microsoft's WebView2 Evergreen bootstrapper

`MicrosoftEdgeWebview2Setup.exe` is Microsoft's own file, unchanged. `silk.exe` embeds it
(`include_bytes!`) and packs it into an installer whose config sets `webview2_bootstrapper = true`
(docs/spec.md § 7).

| | |
|---|---|
| Source | <https://go.microsoft.com/fwlink/p/?LinkId=2124703>, the "Evergreen Bootstrapper" link on <https://developer.microsoft.com/microsoft-edge/webview2#download-the-webview2-runtime> |
| Version | 1.3.275.13 (file and product version; product name "Microsoft Edge Update") |
| SHA-256 | `aa38a8cfce6179b87181609b1c730a29eaf26138fc833af5759e67576770f3a3` |
| Signed by | Microsoft Corporation (Authenticode, valid) |
| Fetched | 2026-10-05 |

## Redistribution

Microsoft's "Distribute your app and the WebView2 Runtime"
(<https://learn.microsoft.com/microsoft-edge/webview2/concepts/distribution>, "Deploying the Evergreen
WebView2 Runtime") offers two ways to ship the bootstrapper: download it at setup time, "Or, download
the bootstrapper and package it with your WebView2 app." Silk takes the second.

The Microsoft Edge WebView2 Runtime License, which the download page shows, lets you copy and
distribute the runtime's object code with your applications, on conditions that bind whoever ships an
installer carrying it:

- the application adds significant primary functionality to it;
- distributors and end users agree to terms that protect it and Microsoft at least as much as that
  licence;
- the code comes directly from Microsoft (this file came from the link above);
- no Microsoft trademark suggests Microsoft endorses the application.

So an author who turns the option on takes those terms on for their installer.

## Refreshing it

The bootstrapper fetches the current runtime whatever its own age, so a refresh is needed only when
Microsoft retires this one. Download it from the source link, check its signature
(`Get-AuthenticodeSignature`), and update the version, hash and date above.
