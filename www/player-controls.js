/**
 * PlayerControls — Canvas-integrated overlay controls (YouTube/VLC style)
 *
 * Creates an HTML overlay on top of the player canvas with:
 * - Bottom bar: play/pause, seek bar, time display, volume slider, fullscreen
 * - Center overlay: loading/buffering/seeking/error messages
 * - Auto-hide after 5s of inactivity (controls + cursor)
 * - Keyboard shortcuts (Space, F, M, arrows)
 * - Library API: on/off/trigger for extensibility
 */
export class PlayerControls {
    /**
     * @param {HTMLElement} container — the .player-container div wrapping the canvas
     * @param {object} [options]
     * @param {number} [options.hideDelayMs=5000] — ms before auto-hide
     */
    constructor(container, options = {}) {
        this.container = container;
        this.hideDelayMs = options.hideDelayMs ?? 5000;

        // State
        this._playing = false;
        this._durationMs = 0;
        this._currentMs = 0;
        this._volume = parseFloat(localStorage.getItem('player-volume') ?? '1');
        this._muted = localStorage.getItem('player-muted') === 'true';
        this._seeking = false;
        this._bufferedMs = 0;
        this._hideTimer = null;
        this._controlsVisible = true;

        // Event listeners registry (Library API)
        this._listeners = {};

        // Build DOM
        this._build();
        this._bindEvents();
        this._applyVolume();
        this._showControls();
    }

    // =========================================================================
    // DOM Construction
    // =========================================================================

    _build() {
        // Wrapper overlay — covers entire container
        this._overlay = document.createElement('div');
        this._overlay.className = 'pc-overlay';

        // --- Center message overlay (Loading, Buffering, Seeking, Error) ---
        this._centerMsg = document.createElement('div');
        this._centerMsg.className = 'pc-center-msg pc-hidden';
        this._centerMsgText = document.createElement('span');
        this._centerMsg.appendChild(this._centerMsgText);
        this._overlay.appendChild(this._centerMsg);

        // --- Bottom gradient ---
        this._gradient = document.createElement('div');
        this._gradient.className = 'pc-gradient';
        this._overlay.appendChild(this._gradient);

        // --- Bottom bar ---
        this._bottomBar = document.createElement('div');
        this._bottomBar.className = 'pc-bottom-bar';

        // Seek bar — multi-layer progress (total / buffered / watched / thumb)
        this._seekRow = document.createElement('div');
        this._seekRow.className = 'pc-seek-row';

        this._seekTrack = document.createElement('div');
        this._seekTrack.className = 'pc-seek-track';

        // Layer 1: buffered (grey)
        this._seekBuffered = document.createElement('div');
        this._seekBuffered.className = 'pc-seek-buffered';
        this._seekTrack.appendChild(this._seekBuffered);

        // Layer 2: watched/played (accent color)
        this._seekPlayed = document.createElement('div');
        this._seekPlayed.className = 'pc-seek-played';
        this._seekTrack.appendChild(this._seekPlayed);

        // Layer 3: thumb (draggable circle)
        this._seekThumb = document.createElement('div');
        this._seekThumb.className = 'pc-seek-thumb';
        this._seekTrack.appendChild(this._seekThumb);

        // Hidden range input for accessibility + easy drag handling
        this._seekBar = document.createElement('input');
        this._seekBar.type = 'range';
        this._seekBar.className = 'pc-seek-input';
        this._seekBar.min = '0';
        this._seekBar.max = '1000';
        this._seekBar.value = '0';
        this._seekBar.step = '1';
        this._seekTrack.appendChild(this._seekBar);

        this._seekRow.appendChild(this._seekTrack);
        this._bottomBar.appendChild(this._seekRow);

        // Controls row
        this._controlsRow = document.createElement('div');
        this._controlsRow.className = 'pc-controls-row';

        // Play/Pause button
        this._playBtn = document.createElement('button');
        this._playBtn.className = 'pc-btn pc-play-btn';
        this._playBtn.title = 'Play';
        this._playBtn.innerHTML = '&#9654;'; // ▶
        this._controlsRow.appendChild(this._playBtn);

        // Time display
        this._timeDisplay = document.createElement('span');
        this._timeDisplay.className = 'pc-time';
        this._timeDisplay.textContent = '0:00 / 0:00';
        this._controlsRow.appendChild(this._timeDisplay);

        // Spacer
        const spacer = document.createElement('div');
        spacer.className = 'pc-spacer';
        this._controlsRow.appendChild(spacer);

        // Volume button
        this._volumeBtn = document.createElement('button');
        this._volumeBtn.className = 'pc-btn pc-volume-btn';
        this._volumeBtn.title = 'Mute';
        this._updateVolumeIcon();
        this._controlsRow.appendChild(this._volumeBtn);

        // Volume slider
        this._volumeSlider = document.createElement('input');
        this._volumeSlider.type = 'range';
        this._volumeSlider.className = 'pc-volume-slider';
        this._volumeSlider.min = '0';
        this._volumeSlider.max = '100';
        this._volumeSlider.value = String(Math.round(this._volume * 100));
        this._controlsRow.appendChild(this._volumeSlider);

        // Fullscreen button
        this._fsBtn = document.createElement('button');
        this._fsBtn.className = 'pc-btn pc-fs-btn';
        this._fsBtn.title = 'Fullscreen';
        this._fsBtn.innerHTML = PlayerControls._svgIcon('fullscreen-enter');
        this._controlsRow.appendChild(this._fsBtn);

        this._bottomBar.appendChild(this._controlsRow);
        this._overlay.appendChild(this._bottomBar);

        // Mount into container
        this.container.style.position = 'relative';
        this.container.appendChild(this._overlay);
    }

    // =========================================================================
    // Event Binding
    // =========================================================================

    _bindEvents() {
        // Play/Pause
        this._playBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            this._trigger(this._playing ? 'pause' : 'play');
        });

        // Seek bar — dragging
        this._seekBar.addEventListener('input', () => {
            if (this._durationMs <= 0) return;
            this._seeking = true;
            const targetMs = Math.round((this._seekBar.value / 1000) * this._durationMs);
            this._updateTimeText(targetMs, this._durationMs);
            this._updateSeekVisuals(targetMs);
        });

        this._seekBar.addEventListener('change', () => {
            if (this._durationMs <= 0) return;
            const targetMs = Math.round((this._seekBar.value / 1000) * this._durationMs);
            // Keep _seeking=true until the player confirms via seekComplete().
            // This prevents TimeUpdate events from snapping the bar back to the
            // old position while the async seek is in progress.
            this._trigger('seek', { targetMs });
        });

        // Volume button (mute toggle)
        this._volumeBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            this._muted = !this._muted;
            localStorage.setItem('player-muted', String(this._muted));
            this._applyVolume();
            this._trigger('volumechange', { volume: this._muted ? 0 : this._volume, muted: this._muted });
        });

        // Volume slider
        this._volumeSlider.addEventListener('input', () => {
            this._volume = parseInt(this._volumeSlider.value, 10) / 100;
            this._muted = this._volume === 0;
            localStorage.setItem('player-volume', String(this._volume));
            localStorage.setItem('player-muted', String(this._muted));
            this._applyVolume();
            this._trigger('volumechange', { volume: this._volume, muted: this._muted });
        });

        // Fullscreen
        this._fsBtn.addEventListener('click', (e) => {
            e.stopPropagation();
            this.toggleFullscreen();
        });

        // Fullscreen change (update icon)
        document.addEventListener('fullscreenchange', () => {
            const isFs = !!document.fullscreenElement;
            this._fsBtn.innerHTML = isFs
                ? PlayerControls._svgIcon('fullscreen-exit')
                : PlayerControls._svgIcon('fullscreen-enter');
            this._fsBtn.title = isFs ? 'Exit Fullscreen' : 'Fullscreen';
            this._trigger('fullscreenchange', { fullscreen: isFs });
        });

        // Auto-hide: mouse move / touch on overlay
        this._overlay.addEventListener('mousemove', () => this._showControls());
        this._overlay.addEventListener('touchstart', () => this._showControls(), { passive: true });

        // Click on overlay (not on controls) → toggle play/pause
        this._overlay.addEventListener('click', (e) => {
            // Ignore clicks on interactive controls (buttons, sliders, bottom bar)
            if (this._bottomBar.contains(e.target)) return;
            this._trigger(this._playing ? 'pause' : 'play');
        });

        // Keyboard shortcuts (on container to capture when focused)
        this.container.tabIndex = this.container.tabIndex === -1 ? 0 : this.container.tabIndex;
        this.container.addEventListener('keydown', (e) => this._onKeyDown(e));

        // Prevent seek/volume sliders from stealing focus oddly
        this._seekBar.addEventListener('mousedown', (e) => e.stopPropagation());
        this._volumeSlider.addEventListener('mousedown', (e) => e.stopPropagation());
    }

    _onKeyDown(e) {
        // Don't capture if user is typing in an input elsewhere
        if (e.target.tagName === 'INPUT' && e.target !== this._seekBar && e.target !== this._volumeSlider) return;

        switch (e.key) {
            case ' ':
                e.preventDefault();
                this._trigger(this._playing ? 'pause' : 'play');
                break;
            case 'f':
            case 'F':
                e.preventDefault();
                this.toggleFullscreen();
                break;
            case 'm':
            case 'M':
                e.preventDefault();
                this._muted = !this._muted;
                localStorage.setItem('player-muted', String(this._muted));
                this._applyVolume();
                this._trigger('volumechange', { volume: this._muted ? 0 : this._volume, muted: this._muted });
                break;
            case 'ArrowRight':
                e.preventDefault();
                this._trigger('seek', { targetMs: Math.min(this._currentMs + 10000, this._durationMs) });
                break;
            case 'ArrowLeft':
                e.preventDefault();
                this._trigger('seek', { targetMs: Math.max(this._currentMs - 10000, 0) });
                break;
            case 'ArrowUp':
                e.preventDefault();
                this._setVolume(Math.min(this._volume + 0.1, 1));
                break;
            case 'ArrowDown':
                e.preventDefault();
                this._setVolume(Math.max(this._volume - 0.1, 0));
                break;
        }

        this._showControls();
    }

    // =========================================================================
    // Public Methods — called by app.js / player event handler
    // =========================================================================

    /** Update current playback time */
    updateTime(currentMs) {
        this._currentMs = currentMs;
        if (!this._seeking) {
            this._updateTimeText(currentMs, this._durationMs);
            if (this._durationMs > 0) {
                this._seekBar.value = String(Math.round((currentMs / this._durationMs) * 1000));
                this._updateSeekVisuals(currentMs);
            }
        }
    }

    /** Signal that a seek operation completed — unlocks the seek bar.
     *  Call this from the Seeked event handler. */
    seekComplete() {
        console.log('[controls] seekComplete, was seeking=', this._seeking);
        this._seeking = false;
    }

    /** Update total duration */
    updateDuration(durationMs) {
        this._durationMs = durationMs;
        this._updateTimeText(this._currentMs, durationMs);
        this._updateSeekVisuals(this._currentMs);
    }

    /** Update buffered amount (in ms or bytes — used for buffer bar) */
    updateBuffered(bufferedMs) {
        this._bufferedMs = bufferedMs;
        this._updateBufferedVisual();
    }

    /** Update playback status */
    updateStatus(status) {
        const wasPlaying = this._playing;
        this._playing = (status === 'Playing');

        // Update play/pause button
        if (this._playing) {
            this._playBtn.innerHTML = '&#9646;&#9646;'; // ⏸
            this._playBtn.title = 'Pause';
        } else {
            this._playBtn.innerHTML = '&#9654;'; // ▶
            this._playBtn.title = 'Play';
        }

        // Auto-hide only when playing
        if (this._playing && !wasPlaying) {
            this._resetHideTimer();
        } else if (!this._playing) {
            this._clearHideTimer();
            this._showControls();
        }
    }

    /** Show center overlay message */
    showMessage(text) {
        this._centerMsgText.textContent = text;
        this._centerMsg.classList.remove('pc-hidden');
    }

    /** Hide center overlay message */
    hideMessage() {
        this._centerMsg.classList.add('pc-hidden');
    }

    /** Get current volume (0-1) */
    getVolume() {
        return this._muted ? 0 : this._volume;
    }

    /** Check if muted */
    isMuted() {
        return this._muted;
    }

    /** Toggle fullscreen on the container */
    toggleFullscreen() {
        if (document.fullscreenElement) {
            document.exitFullscreen().catch(() => {});
        } else {
            this.container.requestFullscreen().catch(() => {});
        }
    }

    /** Clean up — remove overlay from DOM */
    destroy() {
        this._clearHideTimer();
        if (this._overlay.parentNode) {
            this._overlay.parentNode.removeChild(this._overlay);
        }
        this._listeners = {};
    }

    // =========================================================================
    // Library API — on / off / trigger
    // =========================================================================

    /**
     * Subscribe to a control event.
     * Events: play, pause, seek, volumechange, fullscreenchange
     * @param {string} event
     * @param {Function} callback
     */
    on(event, callback) {
        if (!this._listeners[event]) this._listeners[event] = [];
        this._listeners[event].push(callback);
        return this;
    }

    /**
     * Unsubscribe from a control event.
     * @param {string} event
     * @param {Function} callback
     */
    off(event, callback) {
        const cbs = this._listeners[event];
        if (!cbs) return this;
        this._listeners[event] = cbs.filter(cb => cb !== callback);
        return this;
    }

    /** @internal Emit event to listeners */
    _trigger(event, data = {}) {
        const cbs = this._listeners[event];
        if (cbs) {
            for (const cb of cbs) {
                try { cb(data); } catch (e) { console.error(`[PlayerControls] ${event} handler error:`, e); }
            }
        }
    }

    // =========================================================================
    // Internal Helpers
    // =========================================================================

    _formatTime(ms) {
        if (ms == null || isNaN(ms)) return '0:00';
        const totalSec = Math.floor(ms / 1000);
        const h = Math.floor(totalSec / 3600);
        const m = Math.floor((totalSec % 3600) / 60);
        const s = totalSec % 60;
        if (h > 0) {
            return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
        }
        return `${m}:${String(s).padStart(2, '0')}`;
    }

    _updateTimeText(currentMs, durationMs) {
        this._timeDisplay.textContent = `${this._formatTime(currentMs)} / ${this._formatTime(durationMs)}`;
    }

    /** Update visual layers of the seek bar (played + buffered percentages) */
    _updateSeekVisuals(currentMs) {
        if (this._durationMs <= 0) return;
        const playedPct = Math.min((currentMs / this._durationMs) * 100, 100);

        this._seekPlayed.style.width = `${playedPct}%`;
        this._seekThumb.style.left = `${playedPct}%`;
        this._updateBufferedVisual();
    }

    /** Update only the buffered bar — safe to call during seek */
    _updateBufferedVisual() {
        if (this._durationMs <= 0) return;
        const bufferedPct = Math.min((this._bufferedMs / this._durationMs) * 100, 100);
        this._seekBuffered.style.width = `${bufferedPct}%`;
    }

    _setVolume(vol) {
        this._volume = Math.round(vol * 100) / 100;
        this._muted = this._volume === 0;
        this._volumeSlider.value = String(Math.round(this._volume * 100));
        localStorage.setItem('player-volume', String(this._volume));
        localStorage.setItem('player-muted', String(this._muted));
        this._applyVolume();
        this._trigger('volumechange', { volume: this._volume, muted: this._muted });
    }

    _applyVolume() {
        this._updateVolumeIcon();
        this._volumeSlider.value = String(Math.round((this._muted ? 0 : this._volume) * 100));
    }

    _updateVolumeIcon() {
        if (this._muted || this._volume === 0) {
            this._volumeBtn.innerHTML = PlayerControls._svgIcon('volume-muted');
        } else if (this._volume < 0.5) {
            this._volumeBtn.innerHTML = PlayerControls._svgIcon('volume-low');
        } else {
            this._volumeBtn.innerHTML = PlayerControls._svgIcon('volume-high');
        }
    }

    // =========================================================================
    // SVG Icons (flat outline style)
    // =========================================================================

    static _svgIcon(name) {
        const s = (d) =>
            `<svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${d}</svg>`;
        switch (name) {
            case 'fullscreen-enter':
                return s(
                    '<polyline points="15 3 21 3 21 9"/>' +
                    '<polyline points="9 21 3 21 3 15"/>' +
                    '<polyline points="21 15 21 21 15 21"/>' +
                    '<polyline points="3 9 3 3 9 3"/>'
                );
            case 'fullscreen-exit':
                return s(
                    '<polyline points="4 14 10 14 10 20"/>' +
                    '<polyline points="20 10 14 10 14 4"/>' +
                    '<polyline points="14 20 14 14 20 14"/>' +
                    '<polyline points="10 4 10 10 4 10"/>'
                );
            case 'volume-high':
                return s(
                    '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" fill="currentColor" stroke="none"/>' +
                    '<path d="M15.54 8.46a5 5 0 0 1 0 7.07"/>' +
                    '<path d="M19.07 4.93a10 10 0 0 1 0 14.14"/>'
                );
            case 'volume-low':
                return s(
                    '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" fill="currentColor" stroke="none"/>' +
                    '<path d="M15.54 8.46a5 5 0 0 1 0 7.07"/>'
                );
            case 'volume-muted':
                return s(
                    '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" fill="currentColor" stroke="none"/>' +
                    '<line x1="23" y1="9" x2="17" y2="15"/>' +
                    '<line x1="17" y1="9" x2="23" y2="15"/>'
                );
            default:
                return '';
        }
    }

    // =========================================================================
    // Auto-hide
    // =========================================================================

    _showControls() {
        this._controlsVisible = true;
        this._overlay.classList.remove('pc-hide-controls');
        this.container.style.cursor = '';
        this._resetHideTimer();
    }

    _hideControls() {
        if (!this._playing) return; // Never hide when paused
        this._controlsVisible = false;
        this._overlay.classList.add('pc-hide-controls');
        this.container.style.cursor = 'none';
    }

    _resetHideTimer() {
        this._clearHideTimer();
        if (this._playing) {
            this._hideTimer = setTimeout(() => this._hideControls(), this.hideDelayMs);
        }
    }

    _clearHideTimer() {
        if (this._hideTimer !== null) {
            clearTimeout(this._hideTimer);
            this._hideTimer = null;
        }
    }
}
