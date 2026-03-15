# WASM Video Player

A Rust/WASM video player that renders to canvas using WebCodecs API. Supports MP4, MKV, and WebM containers. Designed as a headless library for any frontend framework.

## Architecture

```
Fetch (ReadableStream) -> Demuxer (Rust/WASM) -> WebCodecs (VideoDecoder/AudioDecoder)
                                                       |                    |
                                                 Canvas 2D            Web Audio API
                                                 (drawImage)          (AudioContext)
                                                       +---- AVSync ----+
```

## Prerequisites

- [rustup](https://rustup.rs/) (NOT Homebrew Rust)
- `rustup target add wasm32-unknown-unknown`
- [wasm-pack](https://rustwasm.github.io/wasm-pack/installer/)
- Python 3 (for the dev server)

## Build

```bash
./build.sh
```

## Run (dev)

```bash
./serve.sh
# Open http://localhost:8080
```

## Crates

| Crate | Description |
|-------|-------------|
| `m3u-core` | M3U playlist parser |
| `demuxer` | Multi-format container demuxer (MP4, MKV/WebM) |
| `player-core` | Shared types (PlayerState, PlayerEvent) |
| `player-wasm` | WASM bindings — headless Player API |

## Usage (JS)

```js
import init, { Player } from './pkg/player.js';

await init();
const canvas = document.getElementById('video-canvas');
const player = new Player(canvas);

player.on_event((event) => {
    console.log(event.type, event);
});

await player.load('https://example.com/video.mp4');
await player.play();
```
