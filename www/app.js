import init, { Player } from './pkg/player.js';

// --- DOM Elements ---
const urlInput = document.getElementById('url-input');
const loadBtn = document.getElementById('load-btn');
const playBtn = document.getElementById('play-btn');
const pauseBtn = document.getElementById('pause-btn');
const stopBtn = document.getElementById('stop-btn');
const seekBar = document.getElementById('seek-bar');
const timeCurrent = document.getElementById('time-current');
const timeDuration = document.getElementById('time-duration');
const statusBar = document.getElementById('status-bar');
const errorDisplay = document.getElementById('error-display');
const canvas = document.getElementById('video-canvas');
const overlay = document.getElementById('overlay');
const overlayText = document.getElementById('overlay-text');
const mediaInfoPanel = document.getElementById('media-info');
const mediaInfoList = document.getElementById('media-info-list');
const playlistPanel = document.getElementById('playlist-panel');
const playlistList = document.getElementById('playlist-list');
const downloadBar = document.getElementById('download-bar');

let player = null;
let rafId = null; // requestAnimationFrame handle
let currentStatus = 'Idle'; // Track player status to avoid overlay conflicts
let mediaDurationMs = 0; // Cached duration — avoids calling get_state() inside render_tick callbacks

// --- Helpers ---
function formatTime(ms) {
    if (ms == null || isNaN(ms)) return '0:00';
    const totalSec = Math.floor(ms / 1000);
    const min = Math.floor(totalSec / 60);
    const sec = totalSec % 60;
    return `${min}:${sec.toString().padStart(2, '0')}`;
}

function setStatus(text) {
    statusBar.textContent = `Status: ${text}`;
}

function showError(msg) {
    errorDisplay.textContent = msg;
    errorDisplay.classList.remove('hidden');
}

function hideError() {
    errorDisplay.classList.add('hidden');
}

function showOverlay(text) {
    overlayText.textContent = text;
    overlay.classList.remove('hidden');
}

function hideOverlay() {
    overlay.classList.add('hidden');
}

function setControlsEnabled(play, pause, stop, seek) {
    playBtn.disabled = !play;
    pauseBtn.disabled = !pause;
    stopBtn.disabled = !stop;
    seekBar.disabled = !seek;
}

function displayMediaInfo(info) {
    mediaInfoList.innerHTML = '';
    const items = [];
    if (info.video_codec) items.push(`Video: ${info.video_codec}`);
    if (info.width && info.height) items.push(`Resolution: ${info.width}\u00d7${info.height}`);
    if (info.fps) items.push(`FPS: ${info.fps.toFixed(1)}`);
    if (info.audio_codec) items.push(`Audio: ${info.audio_codec}`);
    if (info.sample_rate) items.push(`Sample rate: ${info.sample_rate} Hz`);
    if (info.channels) items.push(`Channels: ${info.channels}`);
    if (info.duration_ms) items.push(`Duration: ${formatTime(info.duration_ms)}`);

    for (const text of items) {
        const li = document.createElement('li');
        li.textContent = text;
        mediaInfoList.appendChild(li);
    }
    mediaInfoPanel.classList.remove('hidden');
}

// --- Render Loop ---
function startRenderLoop() {
    stopRenderLoop();
    function tick() {
        if (!player) return;
        try {
            const shouldContinue = player.render_tick();
            if (shouldContinue) {
                rafId = requestAnimationFrame(tick);
            } else {
                rafId = null;
            }
        } catch (e) {
            console.error('[render_tick] Exception caught — restarting loop:', e);
            // Don't let the render loop die on transient errors
            rafId = requestAnimationFrame(tick);
        }
    }
    rafId = requestAnimationFrame(tick);
}

function stopRenderLoop() {
    if (rafId !== null) {
        cancelAnimationFrame(rafId);
        rafId = null;
    }
}

// --- Event Handler ---
function handlePlayerEvent(event) {
    if (!event || !event.type) return;

    switch (event.type) {
        case 'StatusChanged':
            currentStatus = event.status;
            setStatus(event.status);
            switch (event.status) {
                case 'Loading':
                    showOverlay('Loading...');
                    setControlsEnabled(false, false, false, false);
                    break;
                case 'Ready':
                    hideOverlay();
                    setControlsEnabled(true, false, true, true);
                    break;
                case 'Playing':
                    hideOverlay();
                    setControlsEnabled(false, true, true, true);
                    break;
                case 'Paused':
                    stopRenderLoop();
                    setControlsEnabled(true, false, true, true);
                    break;
                case 'Buffering':
                    showOverlay('Buffering...');
                    setControlsEnabled(false, true, true, true);
                    break;
                case 'Seeking':
                    showOverlay('Seeking...');
                    break;
                case 'Stopped':
                    stopRenderLoop();
                    setControlsEnabled(true, false, false, false);
                    timeCurrent.textContent = '0:00';
                    seekBar.value = 0;
                    break;
                case 'Error':
                    stopRenderLoop();
                    hideOverlay();
                    setControlsEnabled(false, false, true, false);
                    break;
            }
            break;

        case 'MediaLoaded':
            if (event.info) {
                displayMediaInfo(event.info);
                if (event.info.duration_ms) {
                    mediaDurationMs = event.info.duration_ms;
                    timeDuration.textContent = formatTime(mediaDurationMs);
                }
            }
            break;

        case 'TimeUpdate':
            if (event.current_ms != null) {
                timeCurrent.textContent = formatTime(event.current_ms);
                // Use cached duration — calling get_state() here would cause a
                // RefCell double-borrow panic because we're inside render_tick's &mut self.
                if (mediaDurationMs > 0) {
                    seekBar.value = Math.round((event.current_ms / mediaDurationMs) * 1000);
                }
            }
            break;

        case 'DownloadProgress': {
            const received = event.received_bytes;
            const total = event.total_bytes;
            // Only show overlay during initial loading (before header is parsed).
            // Once status moves to Ready/Playing/etc, show progress in status bar only.
            if (currentStatus === 'Loading') {
                if (total > 0) {
                    const pct = Math.round((received / total) * 100);
                    const mb = (received / 1048576).toFixed(1);
                    const totalMb = (total / 1048576).toFixed(1);
                    showOverlay(`Loading... ${pct}% (${mb} / ${totalMb} MB)`);
                } else {
                    const mb = (received / 1048576).toFixed(1);
                    showOverlay(`Loading... ${mb} MB`);
                }
            }
            // Always update the download bar if it exists
            if (downloadBar && total > 0) {
                downloadBar.style.width = `${Math.round((received / total) * 100)}%`;
            }
            break;
        }

        case 'VideoResized':
            console.log(`Video resized: ${event.width}\u00d7${event.height}`);
            break;

        case 'Error':
            showError(`${event.recoverable ? '\u26a0' : '\u2716'} ${event.message}`);
            break;

        case 'PlaylistTrackChanged':
            updatePlaylistUI(event.index);
            break;

        case 'Seeking':
            showOverlay(`Seeking to ${formatTime(event.target_ms)}...`);
            break;

        case 'Seeked':
            hideOverlay();
            if (event.actual_ms != null) {
                timeCurrent.textContent = formatTime(event.actual_ms);
            }
            break;

        case 'BufferUpdate':
            break;

        case 'Ended':
            stopRenderLoop();
            setStatus('Ended');
            setControlsEnabled(true, false, false, false);
            break;

        default:
            console.log('Player event:', event);
    }
}

// --- Playlist UI ---
function isM3uUrl(url) {
    const path = url.toLowerCase().split('?')[0].split('#')[0];
    return path.endsWith('.m3u') || path.endsWith('.m3u8');
}

function displayPlaylist(playlist) {
    if (!playlist || !playlist.entries || playlist.entries.length === 0) {
        playlistPanel.classList.add('hidden');
        return;
    }
    playlistList.innerHTML = '';
    playlist.entries.forEach((entry, i) => {
        const li = document.createElement('li');
        const label = entry.title || entry.url.split('/').pop() || `Track ${i + 1}`;
        const dur = entry.duration_secs ? ` (${formatTime(entry.duration_secs * 1000)})` : '';
        li.textContent = `${i + 1}. ${label}${dur}`;
        li.dataset.index = i;
        li.addEventListener('click', () => playTrack(i));
        playlistList.appendChild(li);
    });
    playlistPanel.classList.remove('hidden');
}

function updatePlaylistUI(activeIndex) {
    const items = playlistList.querySelectorAll('li');
    items.forEach((li, i) => {
        li.classList.toggle('active', i === activeIndex);
    });
}

async function playTrack(index) {
    if (!player) return;
    try {
        stopRenderLoop();
        await player.play_track(index);
        // Auto-play after track load
        await player.play();
        startRenderLoop();
    } catch (e) {
        showError(`Track load failed: ${e}`);
    }
}

// --- Actions ---
async function loadMedia() {
    const url = urlInput.value.trim();
    if (!url) return;

    hideError();
    stopRenderLoop();

    // Destroy previous player if any
    if (player) {
        player.destroy();
    }

    try {
        player = new Player(canvas);
        player.on_event(handlePlayerEvent);
        currentStatus = 'Loading';
        setStatus('Loading...');
        showOverlay('Loading...');

        if (isM3uUrl(url)) {
            await player.load_playlist(url);
            const playlist = player.get_playlist();
            displayPlaylist(playlist);
        } else {
            playlistPanel.classList.add('hidden');
            await player.load(url);
        }

        // Auto-play: start playback immediately after load
        await player.play();
        startRenderLoop();
    } catch (e) {
        showError(`Load failed: ${e}`);
        setStatus('Error');
        hideOverlay();
    }
}

async function play() {
    if (!player) return;
    try {
        await player.play();
        startRenderLoop();
    } catch (e) {
        showError(`Play failed: ${e}`);
    }
}

async function pause() {
    if (!player) return;
    await player.pause();
    // render loop is stopped in the event handler
}

function stop() {
    if (!player) return;
    stopRenderLoop();
    player.stop();
}

// --- Seek ---
let isSeeking = false;

seekBar.addEventListener('input', () => {
    // While dragging, update the time display but don't seek yet
    if (!player || mediaDurationMs <= 0) return;
    const targetMs = Math.round((seekBar.value / 1000) * mediaDurationMs);
    timeCurrent.textContent = formatTime(targetMs);
});

seekBar.addEventListener('change', async () => {
    if (!player || isSeeking || mediaDurationMs <= 0) return;

    const targetMs = Math.round((seekBar.value / 1000) * mediaDurationMs);
    const wasPlaying = currentStatus === 'Playing' || currentStatus === 'Buffering';
    isSeeking = true;
    try {
        await player.seek(BigInt(targetMs));
        // Restart render loop if we were playing
        if (wasPlaying) {
            startRenderLoop();
        }
    } catch (e) {
        showError(`Seek failed: ${e}`);
    }
    isSeeking = false;
});

// --- Bind Events ---
loadBtn.addEventListener('click', loadMedia);
urlInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') loadMedia();
});
playBtn.addEventListener('click', play);
pauseBtn.addEventListener('click', pause);
stopBtn.addEventListener('click', stop);

// --- Init WASM ---
async function main() {
    try {
        setStatus('Initializing WASM...');
        await init();
        setStatus('Ready \u2014 enter a video URL');
        loadBtn.disabled = false;
    } catch (e) {
        showError(`WASM init failed: ${e}`);
        setStatus('Init error');
    }
}

main();
