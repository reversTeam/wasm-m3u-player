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

let player = null;

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
    if (info.width && info.height) items.push(`Resolution: ${info.width}×${info.height}`);
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

// --- Event Handler ---
function handlePlayerEvent(event) {
    if (!event || !event.type) return;

    switch (event.type) {
        case 'StatusChanged':
            setStatus(event.status);
            switch (event.status) {
                case 'Loading':
                    showOverlay('Loading...');
                    setControlsEnabled(false, false, false, false);
                    break;
                case 'Ready':
                    hideOverlay();
                    setControlsEnabled(true, false, true, false);
                    break;
                case 'Playing':
                    hideOverlay();
                    setControlsEnabled(false, true, true, false);
                    break;
                case 'Paused':
                    setControlsEnabled(true, false, true, false);
                    break;
                case 'Stopped':
                    setControlsEnabled(true, false, false, false);
                    timeCurrent.textContent = '0:00';
                    seekBar.value = 0;
                    break;
                case 'Error':
                    hideOverlay();
                    setControlsEnabled(false, false, true, false);
                    break;
            }
            break;

        case 'MediaLoaded':
            if (event.info) {
                displayMediaInfo(event.info);
                if (event.info.duration_ms) {
                    timeDuration.textContent = formatTime(event.info.duration_ms);
                }
            }
            break;

        case 'TimeUpdate':
            if (event.current_ms != null) {
                timeCurrent.textContent = formatTime(event.current_ms);
                const state = player?.get_state();
                if (state?.duration_ms) {
                    seekBar.value = Math.round((event.current_ms / state.duration_ms) * 1000);
                }
            }
            break;

        case 'VideoResized':
            // Canvas auto-resizes via CSS, but log it
            console.log(`Video resized: ${event.width}×${event.height}`);
            break;

        case 'Error':
            showError(`${event.recoverable ? '⚠' : '✖'} ${event.message}`);
            break;

        case 'PlaylistTrackChanged':
            updatePlaylistUI(event.index);
            break;

        case 'Ended':
            setStatus('Ended');
            setControlsEnabled(true, false, false, false);
            break;

        default:
            console.log('Player event:', event);
    }
}

// --- Playlist UI ---
function isM3uUrl(url) {
    // Strip query string / fragment before checking extension
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
        await player.play_track(index);
    } catch (e) {
        showError(`Track load failed: ${e}`);
    }
}

// --- Actions ---
async function loadMedia() {
    const url = urlInput.value.trim();
    if (!url) return;

    hideError();

    // Destroy previous player if any
    if (player) {
        player.destroy();
    }

    try {
        player = new Player(canvas);
        player.on_event(handlePlayerEvent);
        setStatus('Loading...');
        showOverlay('Loading...');

        if (isM3uUrl(url)) {
            await player.load_playlist(url);
            // Show playlist panel
            const playlist = player.get_playlist();
            displayPlaylist(playlist);
        } else {
            playlistPanel.classList.add('hidden');
            await player.load(url);
        }
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
    } catch (e) {
        showError(`Play failed: ${e}`);
    }
}

function pause() {
    if (!player) return;
    player.pause();
}

function stop() {
    if (!player) return;
    player.stop();
}

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
        setStatus('Ready — enter a video URL');
        loadBtn.disabled = false;
    } catch (e) {
        showError(`WASM init failed: ${e}`);
        setStatus('Init error');
    }
}

main();
