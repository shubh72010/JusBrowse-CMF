# JusBrowse for CMF Watch Pro 2

A small browser client: type a search or an `https://` URL with the on-screen keyboard, the phone fetches it over
FMC Companion, the watch shows the text. Search results include their text, so tapping is instant; tap a preview
again to fetch the full article.

## Build

Needs `git`, `uv`, Rust with `rustup target add thumbv7em-none-eabi` and `rustup component add llvm-tools`, and
`resvg`. The build clones [cmf-watch-sdk](https://github.com/cmf-tech-community/cmf-watch-sdk) at the pinned commit
and uses its packer; nothing prebuilt is committed.

```bash
make build      # -> jusbrowse.wp2app
```

`make clean` removes the SDK checkout and the built package. If your watch runs a newer stub/loader, bump `SDK_REV`
to a matching SDK commit (the pinned one is the hardware-proven stub-1 build).

## Install

The watch needs the patched firmware and App Loader from the [FMC firmware shelf](https://fmc.freethinkel.dev/firmware).
Install `jusbrowse.wp2app` with `wp2apps install jusbrowse.wp2app --address <watch>`, or on Android with
[WP2 Sideloader](https://github.com/MrMontic/wp2-sideloader): forget the watch in Nothing X, leave phone Bluetooth on,
pick the package in the sideloader.

Open the app near the phone with FMC Companion running and allow its `http:en.wikipedia.org` request in the
Companion's Permissions tab. The watch font is ASCII-only; pages over the companion's 2 KB body cap answer "Too large".
