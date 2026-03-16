import init, { Player, player_is_seeking } from './pkg/player.js';
import { PlayerControls } from './player-controls.js';

// --- DOM Elements ---
const urlInput = document.getElementById('url-input');
const loadBtn = document.getElementById('load-btn');
const statusBar = document.getElementById('status-bar');
const errorDisplay = document.getElementById('error-display');
const canvas = document.getElementById('video-canvas');
const container = document.getElementById('player-container');
const mediaInfoPanel = document.getElementById('media-info');
const mediaInfoList = document.getElementById('media-info-list');
const playlistPanel = document.getElementById('playlist-panel');
const playlistList = document.getElementById('playlist-list');
const fileInput = document.getElementById('file-input');

let player = null;
let controls = null;
let rafId = null;
let currentStatus = 'Idle';
let totalFileBytes = 0;
let currentObjectUrl = null;

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
        // Skip render_tick during async seek — avoids RefCell aliasing panic
        // (seek holds &mut self across .await while rAF fires render_tick)
        if (player_is_seeking()) {
            rafId = requestAnimationFrame(tick);
            return;
        }
        try {
            const shouldContinue = player.render_tick();
            if (shouldContinue) {
                rafId = requestAnimationFrame(tick);
            } else {
                rafId = null;
            }
        } catch (e) {
            console.error('[render_tick] Exception caught — restarting loop:', e);
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

// --- Player Event Handler ---
function handlePlayerEvent(event) {
    if (!event || !event.type) return;

    switch (event.type) {
        case 'StatusChanged':
            currentStatus = event.status;
            setStatus(event.status);
            if (controls) controls.updateStatus(event.status);

            switch (event.status) {
                case 'Loading':
                    if (controls) controls.showMessage('Loading...');
                    break;
                case 'Ready':
                    if (controls) controls.hideMessage();
                    break;
                case 'Playing':
                    if (controls) controls.hideMessage();
                    break;
                case 'Paused':
                    stopRenderLoop();
                    break;
                case 'Buffering':
                    if (controls) controls.showMessage('Buffering...');
                    break;
                case 'Seeking':
                    if (controls) controls.showMessage('Seeking...');
                    break;
                case 'Stopped':
                    stopRenderLoop();
                    break;
                case 'Error':
                    stopRenderLoop();
                    if (controls) controls.hideMessage();
                    break;
            }
            break;

        case 'MediaLoaded':
            if (event.info) {
                displayMediaInfo(event.info);
                if (event.info.duration_ms && controls) {
                    controls.updateDuration(event.info.duration_ms);
                }
            }
            break;

        case 'TimeUpdate':
            if (event.current_ms != null && controls) {
                controls.updateTime(event.current_ms);
            }
            break;

        case 'DownloadProgress': {
            const received = event.received_bytes;
            const total = event.total_bytes;
            if (total > 0) totalFileBytes = total;
            if (currentStatus === 'Loading' && controls) {
                if (total > 0) {
                    const pct = Math.round((received / total) * 100);
                    const mb = (received / 1048576).toFixed(1);
                    const totalMb = (total / 1048576).toFixed(1);
                    controls.showMessage(`Loading... ${pct}% (${mb} / ${totalMb} MB)`);
                } else {
                    const mb = (received / 1048576).toFixed(1);
                    controls.showMessage(`Loading... ${mb} MB`);
                }
            }
            // Update buffer bar (received bytes → estimated buffered duration)
            if (controls && total > 0 && controls._durationMs > 0) {
                const bufferedMs = (received / total) * controls._durationMs;
                controls.updateBuffered(bufferedMs);
            }
            break;
        }

        case 'VideoResized':
            break;

        case 'Error':
            showError(`${event.recoverable ? '\u26a0' : '\u2716'} ${event.message}`);
            break;

        case 'PlaylistTrackChanged':
            updatePlaylistUI(event.index);
            break;

        case 'Seeking':
            if (controls) controls.showMessage(`Seeking to ${formatTime(event.target_ms)}...`);
            break;

        case 'Seeked':
            if (controls) {
                controls.hideMessage();
                controls.seekComplete();
            }
            break;

        case 'BufferUpdate':
            // buffered_ms is actually bytes — estimate buffered duration proportionally
            if (controls && totalFileBytes > 0 && controls._durationMs > 0) {
                const bufferedMs = (event.buffered_ms / totalFileBytes) * controls._durationMs;
                controls.updateBuffered(bufferedMs);
            }
            break;

        case 'Ended':
            stopRenderLoop();
            setStatus('Ended');
            if (controls) controls.updateStatus('Ended');
            break;

        default:
            break;
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
        await player.play();
        startRenderLoop();
    } catch (e) {
        showError(`Track load failed: ${e}`);
    }
}

// --- Setup Controls ---
function setupControls() {
    if (controls) controls.destroy();

    controls = new PlayerControls(container);

    controls.on('play', async () => {
        if (!player) return;
        try {
            await player.play();
            startRenderLoop();
        } catch (e) {
            showError(`Play failed: ${e}`);
        }
    });

    controls.on('pause', async () => {
        if (!player) return;
        await player.pause();
    });

    controls.on('seek', async ({ targetMs }) => {
        if (!player) return;
        try {
            await player.seek(BigInt(targetMs));
            if (currentStatus === 'Playing' || currentStatus === 'Buffering') {
                startRenderLoop();
            }
        } catch (e) {
            showError(`Seek failed: ${e}`);
            // Unlock seek bar on failure so it doesn't stay stuck
            if (controls) controls.seekComplete();
        }
    });

    controls.on('volumechange', ({ volume, muted }) => {
        if (!player) return;
        player.set_volume(muted ? 0 : volume);
    });

    // Apply initial volume from localStorage
    if (player) {
        player.set_volume(controls.getVolume());
    }
}

// --- Load Media ---
async function loadMedia(urlOverride) {
    const url = urlOverride || urlInput.value.trim();
    if (!url) return;

    hideError();
    stopRenderLoop();
    totalFileBytes = 0;

    // Revoke previous Object URL to free memory
    if (currentObjectUrl) {
        URL.revokeObjectURL(currentObjectUrl);
        currentObjectUrl = null;
    }

    if (player) {
        player.destroy();
    }

    try {
        player = new Player(canvas);
        player.on_event(handlePlayerEvent);
        setupControls();

        currentStatus = 'Loading';
        setStatus('Loading...');
        controls.showMessage('Loading...');

        if (isM3uUrl(url)) {
            await player.load_playlist(url);
            const playlist = player.get_playlist();
            displayPlaylist(playlist);
        } else {
            playlistPanel.classList.add('hidden');
            await player.load(url);
        }

        await player.play();
        // Apply initial volume from controls (localStorage)
        player.set_volume(controls.getVolume());
        startRenderLoop();
    } catch (e) {
        showError(`Load failed: ${e}`);
        setStatus('Error');
        if (controls) controls.hideMessage();
    }
}

// --- Bind Events ---
loadBtn.addEventListener('click', () => loadMedia());
urlInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') loadMedia();
});

fileInput.addEventListener('change', () => {
    const file = fileInput.files[0];
    if (!file) return;
    currentObjectUrl = URL.createObjectURL(file);
    urlInput.value = file.name;
    loadMedia(currentObjectUrl);
    fileInput.value = ''; // reset so same file can be re-selected
});

// Drag & drop on player container
container.addEventListener('dragover', (e) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
    container.classList.add('drag-over');
});
container.addEventListener('dragleave', () => container.classList.remove('drag-over'));
container.addEventListener('drop', (e) => {
    e.preventDefault();
    container.classList.remove('drag-over');
    const file = e.dataTransfer.files[0];
    if (!file) return;
    currentObjectUrl = URL.createObjectURL(file);
    urlInput.value = file.name;
    loadMedia(currentObjectUrl);
});

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
