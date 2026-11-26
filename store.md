JusBrowse is a minimal text browser for the CMF Watch Pro 2. The watch is the frontend; the phone does the fetching.

Pick a bookmark, press to load it, crown to scroll the text, button to go back.

## Requirements

The [patched firmware](/firmware) and the [App Loader](/firmware). Needs [FMC Companion](https://github.com/fmctools/companion) on your phone, with the `http:<host>` capability allowed for each site you open. Only HTTPS pages are fetched, and only the first ~2 KB of text is shown.