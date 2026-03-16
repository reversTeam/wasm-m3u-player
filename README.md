# WASM Video Player

A high-performance, headless video player built with Rust and WebAssembly. Uses the [WebCodecs API](https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API) for hardware-accelerated decoding and renders to any `<canvas>` element — zero DOM opinions, works with any frontend framework.

## Architecture

```
                         ┌─────────────────────────────────────┐
                         │           Browser (JS)              │
                         │                                     │
  URL / File ──→  fetch (ReadableStream)                       │
                         │                                     │
                         ▼                                     │
              ┌─────────────────────┐                          │
              │   Demuxer (WASM)    │                          │
              │  MP4 · MKV · WebM   │                          │
              └────────┬────────────┘                          │
                       │  Demuxed chunks                       │
              ┌────────┴────────────┐                          │
              ▼                     ▼                          │
   ┌──────────────────┐  ┌──────────────────┐                 │
   │  VideoDecoder     │  │  AudioDecoder     │  ← WebCodecs   │
   │  (hardware accel) │  │  + AC-3 software  │                │
   └────────┬─────────┘  └────────┬─────────┘                 │
            │                     │                            │
            ▼                     ▼                            │
     Canvas 2D             Web Audio API                       │
     (drawImage)           (AudioContext)                       │
            │                     │                            │
            └────── A/V Sync ─────┘                            │
                  (PTS-based)                                  │
                         │                                     │
                         │  Events (StatusChanged, TimeUpdate…)│
                         └─────────────────────────────────────┘
```

**Key design choices:**
- **Headless** — no UI, no controls, no DOM elements. You bring your own UI.
- **Streaming** — download runs in background via `spawn_local`; frames are demuxed, decoded, and rendered progressively in `requestAnimationFrame`.
- **Range-first seeking** — builds a `SeekIndex` from container metadata, then fetches only the needed byte range for instant seek.
- **AC-3/E-AC-3 software decoder** — pure Rust Dolby Digital decoder for browsers that don't support AC-3 in WebCodecs.

## Supported Formats

| Container | Video Codecs | Audio Codecs |
|-----------|-------------|--------------|
| **MP4** (.mp4, .m4v) | H.264 (AVC), HEVC (H.265), AV1 | AAC, Opus, FLAC, AC-3¹, E-AC-3¹ |
| **MKV** (.mkv) | H.264, HEVC, VP8, VP9, AV1 | AAC, Opus, Vorbis, FLAC, AC-3¹, E-AC-3¹ |
| **WebM** (.webm) | VP8, VP9, AV1 | Opus, Vorbis |

¹ AC-3 and E-AC-3 (Dolby Digital / Dolby Digital Plus) are decoded in software via the built-in `ac3-decode` crate when the browser's WebCodecs doesn't support them natively.

## Prerequisites

- [rustup](https://rustup.rs/) (NOT Homebrew Rust)
- `rustup target add wasm32-unknown-unknown`
- [wasm-pack](https://rustwasm.github.io/wasm-pack/installer/)
- Python 3 (for the dev server)

## Build

```bash
./build.sh
```

This compiles all Rust crates to WASM and copies the output to `www/pkg/`.

## Run (dev)

```bash
./serve.sh
# Open http://localhost:8080
```

## Crates

| Crate | Description |
|-------|-------------|
| `player-wasm` | WASM bindings — the headless `Player` API exposed to JavaScript |
| `demuxer` | Multi-format container demuxer (MP4, MKV/WebM) with SeekIndex |
| `player-core` | Shared types (`PlayerState`, `PlayerEvent`, `PlaybackStatus`) |
| `m3u-core` | M3U/M3U8 playlist parser |
| `ac3-decode` | Pure Rust AC-3 / E-AC-3 audio decoder |

## API Reference

### Initialization

```js
import init, { Player, player_is_seeking } from './pkg/player.js';

// Initialize the WASM module (required once)
await init();
```

### `Player`

```js
const player = new Player(canvas); // canvas: HTMLCanvasElement
```

| Method | Signature | Description |
|--------|-----------|-------------|
| `load(url)` | `(string) → Promise<void>` | Load a video URL. Detects Range support and uses streaming or Range-first strategy. |
| `load_playlist(url)` | `(string) → Promise<void>` | Load an M3U/M3U8 playlist, then load the first track. |
| `play()` | `() → Promise<void>` | Start playback. Call `load()` first, then start a `requestAnimationFrame` loop. |
| `pause()` | `() → Promise<void>` | Pause playback. Suspends AudioContext immediately. |
| `stop()` | `() → void` | Stop playback and reset state. |
| `seek(time_ms)` | `(BigInt) → Promise<void>` | Seek to position in milliseconds. Uses Range requests when available. |
| `render_tick()` | `() → boolean` | **Call every frame** from `requestAnimationFrame`. Returns `false` when playback ends. |
| `set_volume(vol)` | `(number) → void` | Set volume: `0.0` (muted) to `1.0` (full). |
| `set_config(cfg)` | `(BufferConfig) → void` | Set buffer config. Must be called **before** `load()`. |
| `on_event(cb)` | `(Function) → void` | Register event callback. |
| `off_event()` | `() → void` | Remove event callback. |
| `get_state()` | `() → object` | Snapshot of current player state. |
| `get_playlist()` | `() → object \| null` | Current playlist data. |
| `get_playlist_index()` | `() → number` | Current track index. |
| `play_track(index)` | `(number) → Promise<void>` | Jump to a specific playlist track. |
| `next_track()` | `() → Promise<void>` | Next playlist track. |
| `previous_track()` | `() → Promise<void>` | Previous playlist track. |
| `destroy()` | `() → void` | Release all resources (decoders, audio context, memory). |

### `player_is_seeking()`

```js
if (player_is_seeking()) {
    // Skip render_tick() during async seek to avoid RefCell aliasing panic
}
```

### `BufferConfig`

Fine-tune streaming and decoding behavior:

```js
const config = new BufferConfig();
config.decode_batch_size = 8;      // Max chunks to decode per render_tick (default: 8)
config.demux_batch_size = 32;      // Chunks to demux in one batch (default: 32)
config.min_chunk_queue = 24;       // Min demuxed queue before demuxing more (default: 24)
config.max_video_queue = 120;      // Pause download above this frame count (default: 120)
config.resume_video_queue = 30;    // Resume download below this frame count (default: 30)
config.max_download_rate = 0n;     // Bytes/sec limit, 0 = unlimited (default: 0)

player.set_config(config);
// Then call player.load(...)
```

### Events

Register a callback with `player.on_event(callback)`. The event object always has a `type` field:

| Event | Fields | Description |
|-------|--------|-------------|
| `StatusChanged` | `status` | Player status changed. Values: `Loading`, `Ready`, `Playing`, `Paused`, `Buffering`, `Seeking`, `Stopped`, `Error`, `Ended` |
| `MediaLoaded` | `info` | Media metadata parsed. See `info` fields below. |
| `TimeUpdate` | `current_ms` | Playback position updated (fires every frame). |
| `DownloadProgress` | `received_bytes`, `total_bytes` | Download progress. `total_bytes` may be 0 if unknown. |
| `BufferUpdate` | `buffered_ms` | Buffer level changed. |
| `VideoResized` | `width`, `height` | Video dimensions changed. |
| `Seeking` | `target_ms` | Seek started. |
| `Seeked` | `actual_ms` | Seek completed. |
| `PlaylistTrackChanged` | `index` | Active playlist track changed. |
| `Error` | `message`, `recoverable` | Error occurred. `recoverable: true` = playback can continue. |
| `Ended` | — | Playback reached end of file. |

**`MediaLoaded` info object:**

```js
{
    video_codec: "avc1.64001f",  // Codec string
    width: 1920,
    height: 1080,
    fps: 29.97,
    audio_codec: "mp4a.40.2",
    sample_rate: 48000,
    channels: 2,
    duration_ms: 180000
}
```

## Integration Examples

### Vanilla JavaScript

```html
<canvas id="video" width="1280" height="720"></canvas>
<button id="play-btn">Play</button>
<input type="range" id="seek-bar" min="0" max="100" value="0">

<script type="module">
import init, { Player, player_is_seeking } from './pkg/player.js';

await init();

const canvas = document.getElementById('video');
const player = new Player(canvas);
let rafId = null;

// Event handling
player.on_event((event) => {
    switch (event.type) {
        case 'MediaLoaded':
            document.getElementById('seek-bar').max = event.info.duration_ms;
            break;
        case 'TimeUpdate':
            document.getElementById('seek-bar').value = event.current_ms;
            break;
        case 'Ended':
            cancelAnimationFrame(rafId);
            break;
    }
});

// Render loop
function tick() {
    if (player_is_seeking()) {
        rafId = requestAnimationFrame(tick);
        return;
    }
    if (player.render_tick()) {
        rafId = requestAnimationFrame(tick);
    }
}

// Load and play
await player.load('https://example.com/video.mp4');
await player.play();
rafId = requestAnimationFrame(tick);

// Seek
document.getElementById('seek-bar').addEventListener('input', async (e) => {
    await player.seek(BigInt(e.target.value));
    rafId = requestAnimationFrame(tick);
});

// Pause / Resume
document.getElementById('play-btn').addEventListener('click', async () => {
    const state = player.get_state();
    if (state.status === 'Playing') {
        await player.pause();
        cancelAnimationFrame(rafId);
    } else {
        await player.play();
        rafId = requestAnimationFrame(tick);
    }
});
</script>
```

### React

```tsx
import { useEffect, useRef, useCallback, useState } from 'react';
import init, { Player, player_is_seeking, BufferConfig } from './pkg/player.js';

let wasmReady = false;
const wasmInit = init().then(() => { wasmReady = true; });

interface MediaInfo {
    duration_ms: number;
    video_codec?: string;
    audio_codec?: string;
    width?: number;
    height?: number;
}

export function VideoPlayer({ src }: { src: string }) {
    const canvasRef = useRef<HTMLCanvasElement>(null);
    const playerRef = useRef<Player | null>(null);
    const rafRef = useRef<number>(0);
    const [status, setStatus] = useState('Idle');
    const [currentTime, setCurrentTime] = useState(0);
    const [duration, setDuration] = useState(0);

    // Render loop
    const tick = useCallback(() => {
        const p = playerRef.current;
        if (!p) return;
        if (player_is_seeking()) {
            rafRef.current = requestAnimationFrame(tick);
            return;
        }
        if (p.render_tick()) {
            rafRef.current = requestAnimationFrame(tick);
        }
    }, []);

    // Initialize & load
    useEffect(() => {
        let cancelled = false;

        (async () => {
            await wasmInit;
            if (cancelled || !canvasRef.current) return;

            const player = new Player(canvasRef.current);
            playerRef.current = player;

            player.on_event((event: any) => {
                if (cancelled) return;
                switch (event.type) {
                    case 'StatusChanged': setStatus(event.status); break;
                    case 'TimeUpdate': setCurrentTime(event.current_ms); break;
                    case 'MediaLoaded': setDuration(event.info.duration_ms); break;
                    case 'Ended': cancelAnimationFrame(rafRef.current); break;
                }
            });

            await player.load(src);
            await player.play();
            rafRef.current = requestAnimationFrame(tick);
        })();

        return () => {
            cancelled = true;
            cancelAnimationFrame(rafRef.current);
            playerRef.current?.destroy();
            playerRef.current = null;
        };
    }, [src, tick]);

    const handleSeek = async (e: React.ChangeEvent<HTMLInputElement>) => {
        const player = playerRef.current;
        if (!player) return;
        await player.seek(BigInt(e.target.value));
        rafRef.current = requestAnimationFrame(tick);
    };

    const togglePlay = async () => {
        const player = playerRef.current;
        if (!player) return;
        if (status === 'Playing') {
            await player.pause();
            cancelAnimationFrame(rafRef.current);
        } else {
            await player.play();
            rafRef.current = requestAnimationFrame(tick);
        }
    };

    return (
        <div>
            <canvas ref={canvasRef} width={1280} height={720} />
            <div>
                <button onClick={togglePlay}>
                    {status === 'Playing' ? '⏸' : '▶'}
                </button>
                <input
                    type="range"
                    min={0}
                    max={duration}
                    value={currentTime}
                    onChange={handleSeek}
                />
                <span>{formatTime(currentTime)} / {formatTime(duration)}</span>
            </div>
        </div>
    );
}

function formatTime(ms: number): string {
    const sec = Math.floor(ms / 1000);
    return `${Math.floor(sec / 60)}:${(sec % 60).toString().padStart(2, '0')}`;
}
```

### Vue 3 (Composition API)

```vue
<template>
    <div class="video-player">
        <canvas ref="canvasEl" width="1280" height="720" />
        <div class="controls">
            <button @click="togglePlay">{{ isPlaying ? '⏸' : '▶' }}</button>
            <input
                type="range"
                :min="0"
                :max="duration"
                :value="currentTime"
                @input="onSeek"
            />
            <span>{{ formatTime(currentTime) }} / {{ formatTime(duration) }}</span>
            <input
                type="range"
                min="0"
                max="1"
                step="0.05"
                :value="volume"
                @input="onVolume"
            />
        </div>
    </div>
</template>

<script setup lang="ts">
import { ref, onMounted, onBeforeUnmount, watch } from 'vue';
import init, { Player, player_is_seeking } from './pkg/player.js';

const props = defineProps<{ src: string }>();

const canvasEl = ref<HTMLCanvasElement>();
const isPlaying = ref(false);
const currentTime = ref(0);
const duration = ref(0);
const volume = ref(1);

let player: Player | null = null;
let rafId = 0;

function tick() {
    if (!player) return;
    if (player_is_seeking()) { rafId = requestAnimationFrame(tick); return; }
    if (player.render_tick()) { rafId = requestAnimationFrame(tick); }
}

function formatTime(ms: number): string {
    const sec = Math.floor(ms / 1000);
    return `${Math.floor(sec / 60)}:${(sec % 60).toString().padStart(2, '0')}`;
}

async function togglePlay() {
    if (!player) return;
    if (isPlaying.value) {
        await player.pause();
        cancelAnimationFrame(rafId);
    } else {
        await player.play();
        rafId = requestAnimationFrame(tick);
    }
}

async function onSeek(e: Event) {
    if (!player) return;
    const target = (e.target as HTMLInputElement).value;
    await player.seek(BigInt(target));
    rafId = requestAnimationFrame(tick);
}

function onVolume(e: Event) {
    volume.value = parseFloat((e.target as HTMLInputElement).value);
    player?.set_volume(volume.value);
}

onMounted(async () => {
    await init();
    if (!canvasEl.value) return;

    player = new Player(canvasEl.value);
    player.on_event((event: any) => {
        switch (event.type) {
            case 'StatusChanged':
                isPlaying.value = event.status === 'Playing';
                break;
            case 'TimeUpdate':
                currentTime.value = event.current_ms;
                break;
            case 'MediaLoaded':
                duration.value = event.info.duration_ms;
                break;
        }
    });

    await player.load(props.src);
    await player.play();
    rafId = requestAnimationFrame(tick);
});

onBeforeUnmount(() => {
    cancelAnimationFrame(rafId);
    player?.destroy();
    player = null;
});

watch(() => props.src, async (newSrc) => {
    if (!player || !canvasEl.value) return;
    cancelAnimationFrame(rafId);
    player.destroy();
    player = new Player(canvasEl.value);
    await player.load(newSrc);
    await player.play();
    rafId = requestAnimationFrame(tick);
});
</script>
```

### Svelte

```svelte
<script lang="ts">
    import { onMount, onDestroy } from 'svelte';
    import init, { Player, player_is_seeking } from './pkg/player.js';

    export let src: string;

    let canvas: HTMLCanvasElement;
    let player: Player | null = null;
    let rafId = 0;
    let status = 'Idle';
    let currentTime = 0;
    let duration = 0;

    function tick() {
        if (!player) return;
        if (player_is_seeking()) { rafId = requestAnimationFrame(tick); return; }
        if (player.render_tick()) { rafId = requestAnimationFrame(tick); }
    }

    function formatTime(ms: number): string {
        const sec = Math.floor(ms / 1000);
        return `${Math.floor(sec / 60)}:${(sec % 60).toString().padStart(2, '0')}`;
    }

    async function togglePlay() {
        if (!player) return;
        if (status === 'Playing') {
            await player.pause();
            cancelAnimationFrame(rafId);
        } else {
            await player.play();
            rafId = requestAnimationFrame(tick);
        }
    }

    async function handleSeek(e: Event) {
        if (!player) return;
        await player.seek(BigInt((e.target as HTMLInputElement).value));
        rafId = requestAnimationFrame(tick);
    }

    onMount(async () => {
        await init();
        player = new Player(canvas);

        player.on_event((event: any) => {
            switch (event.type) {
                case 'StatusChanged': status = event.status; break;
                case 'TimeUpdate': currentTime = event.current_ms; break;
                case 'MediaLoaded': duration = event.info.duration_ms; break;
                case 'Ended': cancelAnimationFrame(rafId); break;
            }
        });

        await player.load(src);
        await player.play();
        rafId = requestAnimationFrame(tick);
    });

    onDestroy(() => {
        cancelAnimationFrame(rafId);
        player?.destroy();
    });
</script>

<div class="video-player">
    <canvas bind:this={canvas} width={1280} height={720} />
    <div class="controls">
        <button on:click={togglePlay}>
            {status === 'Playing' ? '⏸' : '▶'}
        </button>
        <input
            type="range"
            min={0}
            max={duration}
            value={currentTime}
            on:input={handleSeek}
        />
        <span>{formatTime(currentTime)} / {formatTime(duration)}</span>
    </div>
</div>
```

### Angular

```typescript
// video-player.component.ts
import { Component, ElementRef, Input, OnInit, OnDestroy, ViewChild } from '@angular/core';
import init, { Player, player_is_seeking } from './pkg/player.js';

@Component({
    selector: 'app-video-player',
    template: `
        <div class="video-player">
            <canvas #videoCanvas width="1280" height="720"></canvas>
            <div class="controls">
                <button (click)="togglePlay()">{{ isPlaying ? '⏸' : '▶' }}</button>
                <input
                    type="range"
                    [min]="0"
                    [max]="duration"
                    [value]="currentTime"
                    (input)="onSeek($event)"
                />
                <span>{{ formatTime(currentTime) }} / {{ formatTime(duration) }}</span>
            </div>
        </div>
    `,
})
export class VideoPlayerComponent implements OnInit, OnDestroy {
    @ViewChild('videoCanvas', { static: true }) canvasRef!: ElementRef<HTMLCanvasElement>;
    @Input() src!: string;

    isPlaying = false;
    currentTime = 0;
    duration = 0;

    private player: Player | null = null;
    private rafId = 0;

    async ngOnInit() {
        await init();
        this.player = new Player(this.canvasRef.nativeElement);

        this.player.on_event((event: any) => {
            switch (event.type) {
                case 'StatusChanged':
                    this.isPlaying = event.status === 'Playing';
                    break;
                case 'TimeUpdate':
                    this.currentTime = event.current_ms;
                    break;
                case 'MediaLoaded':
                    this.duration = event.info.duration_ms;
                    break;
                case 'Ended':
                    cancelAnimationFrame(this.rafId);
                    break;
            }
        });

        await this.player.load(this.src);
        await this.player.play();
        this.startRenderLoop();
    }

    ngOnDestroy() {
        cancelAnimationFrame(this.rafId);
        this.player?.destroy();
    }

    private startRenderLoop() {
        const tick = () => {
            if (!this.player) return;
            if (player_is_seeking()) { this.rafId = requestAnimationFrame(tick); return; }
            if (this.player.render_tick()) { this.rafId = requestAnimationFrame(tick); }
        };
        this.rafId = requestAnimationFrame(tick);
    }

    async togglePlay() {
        if (!this.player) return;
        if (this.isPlaying) {
            await this.player.pause();
            cancelAnimationFrame(this.rafId);
        } else {
            await this.player.play();
            this.startRenderLoop();
        }
    }

    async onSeek(event: Event) {
        if (!this.player) return;
        const value = (event.target as HTMLInputElement).value;
        await this.player.seek(BigInt(value));
        this.startRenderLoop();
    }

    formatTime(ms: number): string {
        const sec = Math.floor(ms / 1000);
        return `${Math.floor(sec / 60)}:${(sec % 60).toString().padStart(2, '0')}`;
    }
}
```

### SolidJS

```tsx
import { createSignal, onMount, onCleanup } from 'solid-js';
import init, { Player, player_is_seeking } from './pkg/player.js';

export function VideoPlayer(props: { src: string }) {
    let canvas!: HTMLCanvasElement;
    let player: Player | null = null;
    let rafId = 0;

    const [status, setStatus] = createSignal('Idle');
    const [currentTime, setCurrentTime] = createSignal(0);
    const [duration, setDuration] = createSignal(0);

    function tick() {
        if (!player) return;
        if (player_is_seeking()) { rafId = requestAnimationFrame(tick); return; }
        if (player.render_tick()) { rafId = requestAnimationFrame(tick); }
    }

    const formatTime = (ms: number) => {
        const sec = Math.floor(ms / 1000);
        return `${Math.floor(sec / 60)}:${(sec % 60).toString().padStart(2, '0')}`;
    };

    onMount(async () => {
        await init();
        player = new Player(canvas);

        player.on_event((event: any) => {
            switch (event.type) {
                case 'StatusChanged': setStatus(event.status); break;
                case 'TimeUpdate': setCurrentTime(event.current_ms); break;
                case 'MediaLoaded': setDuration(event.info.duration_ms); break;
                case 'Ended': cancelAnimationFrame(rafId); break;
            }
        });

        await player.load(props.src);
        await player.play();
        rafId = requestAnimationFrame(tick);
    });

    onCleanup(() => {
        cancelAnimationFrame(rafId);
        player?.destroy();
    });

    return (
        <div>
            <canvas ref={canvas} width={1280} height={720} />
            <div>
                <button onClick={async () => {
                    if (!player) return;
                    if (status() === 'Playing') {
                        await player.pause();
                        cancelAnimationFrame(rafId);
                    } else {
                        await player.play();
                        rafId = requestAnimationFrame(tick);
                    }
                }}>
                    {status() === 'Playing' ? '⏸' : '▶'}
                </button>
                <input
                    type="range"
                    min={0}
                    max={duration()}
                    value={currentTime()}
                    onInput={async (e) => {
                        if (!player) return;
                        await player.seek(BigInt(e.currentTarget.value));
                        rafId = requestAnimationFrame(tick);
                    }}
                />
                <span>{formatTime(currentTime())} / {formatTime(duration())}</span>
            </div>
        </div>
    );
}
```

## M3U Playlist Support

Load `.m3u` / `.m3u8` playlists with track navigation:

```js
await player.load_playlist('https://example.com/playlist.m3u');
await player.play();

// Navigate tracks
await player.next_track();
await player.previous_track();
await player.play_track(3); // Jump to track index 3

// Get playlist info
const playlist = player.get_playlist();
console.log(playlist.entries); // [{ url, title, duration_secs }, ...]
console.log(player.get_playlist_index()); // Current track index
```

## Local File Playback

Load local files via `URL.createObjectURL`:

```js
const input = document.querySelector('input[type="file"]');
input.addEventListener('change', async () => {
    const file = input.files[0];
    const url = URL.createObjectURL(file);
    await player.load(url);
    await player.play();
    // Don't forget: URL.revokeObjectURL(url) when done
});
```

Drag & drop works the same way — create an Object URL from the dropped `File`.

## Important Notes

### The Render Loop

The player **does not manage its own render loop**. You must call `render_tick()` from `requestAnimationFrame`:

```js
function tick() {
    if (player_is_seeking()) {
        // IMPORTANT: skip render_tick during async seek
        // to avoid RefCell aliasing panic in WASM
        requestAnimationFrame(tick);
        return;
    }
    if (player.render_tick()) {
        requestAnimationFrame(tick);
    }
}
requestAnimationFrame(tick);
```

**Always check `player_is_seeking()` before calling `render_tick()`** — calling both concurrently causes a WASM panic due to shared mutable state.

### Seek Uses BigInt

The `seek()` method takes a `BigInt` for millisecond precision:

```js
await player.seek(BigInt(30000)); // Seek to 30 seconds
await player.seek(30000n);       // Same thing with BigInt literal
```

### Cleanup

Always call `destroy()` when removing the player to free WASM memory, close AudioContext, and release decoder resources:

```js
player.destroy();
```

### Browser Requirements

- **WebCodecs API** — Chrome 94+, Edge 94+, Opera 80+, Safari 16.4+
- **WASM** — All modern browsers
- **SharedArrayBuffer** — Not required
- **Web Audio API** — All modern browsers

## License

MIT
