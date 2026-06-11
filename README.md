# Lightshot We Deserve

A clean, open-source screenshot tool for Windows. It lives in the system tray
and gives you global hotkeys for area and full-screen capture — then lets you
annotate, copy, or save in one move. No accounts, no upload servers, no clutter.

Built by the team at **[keepsimple.io](https://keepsimple.io)**.

## Download

Grab the latest build from the [Releases page](https://github.com/manager/lightshot-we-deserve/releases/latest):

- **Portable** — [`lightshot-we-deserve.exe`](https://github.com/manager/lightshot-we-deserve/releases/latest/download/lightshot-we-deserve.exe) · run it directly, no install.
- **Installer** — `Lightshot.We.Deserve_..._x64-setup.exe` on the [Releases page](https://github.com/manager/lightshot-we-deserve/releases/latest) · sets up the app and optional autostart.

## Features

- Area and full-screen capture via global hotkeys
- High-contrast crosshair that stays visible on light and dark backgrounds
- Annotate: pen, line, arrow, rectangle, marker, text, and blur
- Adjustable tool size and color
- Copy to clipboard, save, or "Save as…" with a custom name
- Drag the selection frame to reposition it before capturing
- Saves to your chosen folder (Desktop by default)
- Quietly runs from the tray, optional launch on startup

## Default hotkeys

| Action | Shortcut |
| --- | --- |
| Capture area | `Ctrl + Shift + 4` |
| Capture full screen | `Ctrl + Shift + 3` |

Hotkeys and save folder can be changed in the app settings.

## Build from source

Built with [Tauri](https://tauri.app) (Rust + WebView2). On Windows with the
Rust toolchain and Node installed:

```
npm install
npm run tauri build
```

The Windows installer and portable `.exe` are produced under
`src-tauri/target/release/`.

## License

[MIT](LICENSE) © keepsimple.io
