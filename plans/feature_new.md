# Proposed Configuration & System Enhancements for PicoGallery

This document outlines a comprehensive set of features proposed to enhance the configuration, administration, and performance of PicoGallery end-to-end. These features target the specific challenges of running a lightweight, headless image viewer on Raspberry Pi hardware without a desktop environment.

---

## Top 10 Core Configuration Enhancements

### 1. Web-Based Configuration Dashboard
* **Target Component:** [remote.rs](../src/remote.rs) & [config.rs](../src/config.rs)
* **Goal:** Extend the existing HTTP remote control to serve a `/settings` page.
* **Details:** Provide a responsive web settings form to allow users to configure display settings, manage caches, and enable/reconfigure plugins (like directory, WebDAV, and PhotoPrism) without using SSH or a command-line interface.

### 2. Captive Portal Wi-Fi Bootstrapping (AP Mode)
* **Target Component:** [wifi.rs](../src/wifi.rs)
* **Goal:** Allow headless setups to connect to new Wi-Fi networks easily.
* **Details:** If the frame fails to connect to the configured network at boot, it should spin up a temporary local Wi-Fi Access Point (e.g. `PicoGallery-Setup`) and serve a captive portal. Users can then connect via their phone to select a network and enter a password.

### 3. Remote Setup QR Code on Frame OSD
* **Target Component:** [osd.rs](../src/osd.rs)
* **Goal:** Simplify locating the web interface URL.
* **Details:** Generate and display a QR code on-screen (via the framebuffer OSD) during the initial startup sequence or when the menu is active. Users can scan the QR code to instantly open the web remote or setup portal on their phone.

### 4. Zero-Downtime Hot-Reloading of Configuration
* **Target Component:** [main.rs](../src/main.rs)
* **Goal:** Apply configuration changes without restarting the application.
* **Details:** Use a file-system watcher (e.g. `notify` crate) to watch `config.toml`. When changes occur, hot-reload display options, sleep schedules, and plugin lists in memory without interrupting the active slideshow.

### 5. Safe Mode Diagnostic Screen
* **Target Component:** [config.rs](../src/config.rs) & [renderer.rs](../src/renderer.rs)
* **Goal:** Prevent silent crashes on invalid configurations.
* **Details:** If the config file is corrupted or contains syntax errors, catch the parse error and render a safe-mode diagnostic UI directly to the KMS/DRM framebuffer, detailing the error line and providing troubleshooting instructions.

### 6. Environment Variable Configuration Overrides
* **Target Component:** [config.rs](../src/config.rs)
* **Goal:** Enable container-friendly deployment configuration.
* **Details:** Merge environment variable overrides (e.g. `PICO_DISPLAY_FILL_SCREEN=true`) into the configuration at startup. This enables running containerized environments (Docker, Balena) without altering flat files on host systems.

### 7. Multi-Profile / Playlist Switcher
* **Target Component:** [config.rs](../src/config.rs) & [menu.rs](../src/menu.rs)
* **Goal:** Allow instant slideshow selection swaps.
* **Details:** Support multiple profile config blocks (e.g. `[profiles.family]`, `[profiles.scenic]`). Allow users to switch the active profile dynamically via the web remote or the on-screen settings menu.

### 8. Headless OAuth "Device Authorization" Flow
* **Target Component:** [lib.rs](../core/src/lib.rs) (plugins authentication trait)
* **Goal:** Make cloud plugin setup simple on headless systems.
* **Details:** Implement the OAuth Device Authorization Grant (RFC 8628) for plugins (such as Google Photos/Drive). Instead of forcing a browser open locally, display a verification code and link/QR code on the screen for the user to authorize on another device.

### 9. Graceful Offline Mode & Fallback Slideshow
* **Target Component:** [slideshow.rs](../src/slideshow.rs)
* **Goal:** Prevent black/empty screens when network sources are offline.
* **Details:** Support a configured fallback folder or embedded default pictures. If network-based plugins go offline, automatically transition to a fallback slideshow with a subtle offline status indicator.

### 10. Native GPIO Hardware Buttons & Motion Sensor Mapping
* **Target Component:** [config.rs](../src/config.rs) & [main.rs](../src/main.rs)
* **Goal:** Simplify physical hardware builds.
* **Details:** Add native configuration keys for mapping physical buttons and PIR motion sensors directly to the Raspberry Pi GPIO pins (e.g. mapping next slide, screen sleep, etc.), avoiding the need for external wrapper scripts.

---

## 100 Additional Feature Enhancements (Categorized)

### Category A: Photo & Video Source Integrations (1–10)
1. **Immich Native Client:** Direct integration with Immich server APIs, fetching selected user collections or public albums.
2. **Samba/CIFS Native Mount:** Stream photos directly from local network shares without needing OS-level mount configuration.
3. **SFTP / SSH Sync Support:** Download and sync photos over SFTP from a remote server to local cache folders.
4. **Nextcloud Custom API Integration:** Support tag-based filtering and custom album shares directly from Nextcloud APIs rather than raw WebDAV.
5. **Flickr API Client:** Pull photos from authenticated personal Flickr collections or public groups.
6. **Instagram Feed Importer:** Import and display photos from personal Instagram feeds via Basic Display API.
7. **Apple iCloud Shared Album Support:** Sync photos directly from public iCloud shared stream URLs.
8. **USB Auto-Mount & Scan:** Automatically detect when a USB drive is inserted, mount it, scan for JPEGs, and switch sources.
9. **Dropbox API Client:** Sync from targeted Dropbox folders using a developer token.
10. **OneDrive API Client:** Sync files directly from specific Microsoft OneDrive folders.

### Category B: Image Rendering, Styling & Transitions (11–20)
11. **Smart Auto-Rotation:** Combine EXIF orientation tags with visual aspect-ratio checks to prevent landscape squishing.
12. **Ken Burns Zoom/Pan Target Profiles:** Set focus points (e.g., face boundaries or center-of-gravity details) for Ken Burns effects.
13. **Cross-Fade Transition Custom Curve:** Select transition interpolation curves (linear, ease-in, ease-out, ease-in-out).
14. **Glitch / Pixel-Sort Transitions:** Add digital aesthetic transitions for retro-styled displays.
15. **Mosaic / Pixelation Transition:** Transition slides through an adjustable pixel block scaling pattern.
16. **Edge Dominant Color Borders:** Fill letterbox borders with dynamic complementary solid colors or gradients instead of plain black.
17. **Polaroid Frame Aesthetic Overlay:** Render a polaroid-like paper border, complete with stylized text of date and location tags.
18. **Split-Screen Portrait Rendering:** Automatically display two portrait photos side-by-side on wide/landscape displays.
19. **Auto-Framing / Face Centering:** Auto-detect face regions to center-crop images dynamically rather than using generic center-crop.
20. **Sepia & Vintage Shader Filters:** Add toggleable shaders for dynamic vintage filters.

### Category C: Live Widgets & OSD Overlays (21–30)
21. **Weather Forecast Overlay:** Integrate OpenWeatherMap API to display local forecast information.
22. **News Ticker Feed:** Add a scrolling RSS/Atom feed banner along the bottom of the screen.
23. **Calendar Reminders widget:** Connect to iCal/Google Calendar feeds to show daily agendas.
24. **Spotify Active Media Indicator:** Display track details, artist, and album art from the user's active Spotify session.
25. **Stock & Crypto Price Ribbon:** Scroll selected financial tickers in an overlay.
26. **Motivational / Daily Quotes Overlay:** Render quotes from a locally-defined list or a remote API.
27. **Pi Diagnostics Pill:** Add an optional overlay showing CPU temperature, RAM usage, and network health indicators.
28. **Multi-Timezone Digital Clock:** Render clocks representing configured zones.
29. **Air Quality Index (AQI) Overlay:** Fetch and print local environmental AQI numbers.
30. **Daily Chore Checklist Widget:** Sync and display shared family checklists (e.g. from Trello or Todoist).

### Category D: Performance, Cache & Resource Tuning (31–40)
31. **Sub-Sampling JPEG Decoder:** Tell the decoder to decode high-MP JPEGs directly at half/quarter resolution to preserve Pi Zero RAM.
32. **Network-Adaptive Cache Prefetching:** Scale prefetch queues dynamically depending on network performance.
33. **Adaptive FPS Limiting:** Drop frame rates to 1-2 FPS during static slides, and scale up only during transitions to reduce power consumption.
34. **Memory Watchdog Graceful Handler:** Automatically clear caches and restart slideshow threads if memory usage exceeds threshold limits.
35. **SD Card Wear Mitigation:** Keep configuration buffers in `tmpfs` RAM disk and write to disk only when modifications occur.
36. **Multi-Threaded Resizing Pipeline:** Distribute prefetch resizing tasks across multiple cores (for Pi 3/4/5 platforms).
37. **Dynamic Compression of Cache Files:** Downscale and re-compress cached JPEGs to a configured compression factor (e.g., 80%) to save space.
38. **GPU Shader Acceleration Fallback:** Allow OpenGL ES2 shader execution on newer Pi units.
39. **Decoupled Fetch & Decode Threading:** Isolate download queues so slow networks never block active transition render loops.
40. **LRU Cache Cleanup Policies:** Evict cache files using Least-Recently-Used heuristics based on size, age, and album categories.

### Category E: Smart Sorting, Playback & Curations (41–50)
41. **Multi-Year "On This Day" Smart Boost:** Prioritize showing photos taken on the current month/day in past years.
42. **Same-Day / Same-Event Clustering:** Group sets of photos taken within hours of each other to show mini-event runs.
43. **Color-Harmonious Transitions:** Analyze and chain slides so that the transition matches photos with similar dominant colors.
44. **Favorites Weight Factor:** Apply weighting algorithms that make designated favorites show up more frequently.
45. **Image Deduplication Hash Check:** Compute dHash/aHash values to prevent duplicate or near-identical images from rendering.
46. **Time-of-Day Contextual Sorting:** Play brighter, energetic photos in the morning and calm, low-light landscape photos at night.
47. **Season-Based Slideshow Filtering:** Filter photos by matching month categories to seasons (e.g., winter photos in December).
48. **Geographical Location Clustering:** Sort slides so photos taken in the same city/country play in chunks.
49. **Laplacian Blur Rejection:** Run real-time edge variance calculations to skip blurry or out-of-focus images.
50. **Rating-Based Metadata Filter:** Read rating stars (1-5) from EXIF metadata and filter out lower-rated files.

### Category F: Automation, Energy & Environmental Sensors (51–60)
51. **Light Sensor Auto-Brightness Control:** Read a light resistor over GPIO/I2C to automatically dim or brighten the screen backlight.
52. **PIR Sensor Power Management:** Sleep display after configured inactivity and wake up instantly when motion is detected.
53. **HDMI CEC Control Integration:** Send stand-by/wake commands over HDMI to turn the physical monitor off/on instead of rendering black.
54. **Weekday vs Weekday Schedules:** Support distinct display sleep/wake intervals for weekdays vs weekends.
55. **Sunset-Triggered Sleep Schedules:** Calculate sunset schedules locally to activate night dimming modes dynamically.
56. **Thermal Throttling Guard:** Automatically increase slide intervals or drop FPS if Pi CPU exceeds warning temperature levels.
57. **Display Power HTTP API:** Add a simple REST endpoint to toggle screen power via external scripts.
58. **Low Voltage Safety Shutdown:** Detect under-voltage warnings on the Pi and safely shut down the system to prevent SD corruption.
59. **Presence Detection via Bluetooth/Wi-Fi:** Sleep/wake the frame based on whether a user's phone MAC address is active on the local network.
60. **USB Camera Face Activation:** Wake the screen from standby when a connected USB camera detects a human face in the room.

### Category G: Smart Home & IoT Integrations (61–70)
61. **MQTT Command Client:** Subscribe to command topics (`picogallery/cmd`) to control slideshow states and settings from Home Assistant.
62. **Home Assistant Discovery Support:** Automatically register frame attributes (power, volume, slideshow source) in Home Assistant.
63. **Slide Transition Webhook Emitters:** Send HTTP post requests on slide changes containing photo name, album, and location tags.
64. **Node-RED Node Integration:** Provide integration guides and webhooks to trigger flow actions on Node-RED systems.
65. **Matter Integration Layer:** Wrap status messages in Matter-compatible interfaces for smart display indicators.
66. **Dynamic Ambient Backlight Sync:** Synchronize backend LED strips (Tasmota/WLED) to match the dominant colors of the active slide.
67. **Prometheus Metrics Exporter:** Expose a `/metrics` page sharing temperature, frame rate, CPU, and download stats.
68. **IFTTT Custom Actions:** Fire webhooks when a photo is liked, forwarding notifications or triggering automated tasks.
69. **Alexa Smart Home Skill Integration:** Enable voice commands (e.g., "Alexa, show next photo") via cloud interfaces.
70. **Google Home Action Support:** Integrate display control widgets directly into the Google Home dashboard ecosystem.

### Category H: Web Remote & Administration Extras (71–80)
71. **Config Export & Import Utility:** Backup the active TOML settings configuration into JSON files for easy restore.
72. **Web Drag-and-Drop Media Uploader:** Drag JPEGs straight to local storage from any smartphone browser.
73. **Scrollable Real-Time Log Viewer:** View debugging logs directly inside the settings page without shell tools.
74. **Slideshow Mirroring View:** Stream a low-resolution thumbnail preview of the active screen frame on the web dashboard.
75. **Settings Portal Authentication Guard:** Require token verification or password entry to open configuration screens.
76. **Manual Cache Purging Trigger:** Provide a button to clear downloaded caches and trigger a full folder re-scan.
77. **Advanced Network Configuration Interface:** Modify IP settings (DNS, gateway, DHCP settings) via web menus.
78. **System Power Control Operations:** Safely reboot or shut down the host OS via remote control buttons.
79. **Local mDNS Hostname Support:** Broadcast the frame as `picogallery.local` on LAN networks.
80. **Self-Update OTA Pipelines:** Fetch newer release binaries directly from GitHub releases via the administrator interface.

### Category I: Visual Layouts & Ambient Displays (81–90)
81. **Same-Day Collage Auto-Grouping:** Merge portrait images from same-day folders into dual-split arrangements.
82. **Grid Layout Mode:** Render collections of 4 small thumbnails on a single grid screen periodically.
83. **Film Vignette Screen Shader:** Renders a subtle vignetted dark edge on the display to mimic slide projections.
84. **Custom Clock Dial Displays:** Switch between minimalist text, analog hands, or flip-clock clocks.
85. **Ambient Audio Soundscapes:** Play customizable ambient background tracks (rain, white noise) synchronized to the slideshow.
86. **Color Accent Matching Matrices:** Adjust active display color profiles to match specific frame borders.
87. **Custom TTF Font Ingestion:** Ingest custom local TTF/OTF files to change OSD text rendering styles.
88. **Seasonal Transition Graphic Overlays:** Add light seasonal decorations (snowflakes, autumn leaves) on transition frames.
89. **Ultra-Minimalist Frameless Mode:** Single-button toggle to clear out OSD panels, clocks, margins, and borders.
90. **OLED Pixel-Shifter Protection:** Shift render canvas parameters by a few pixels periodically to avoid burn-in.

### Category J: Diagnostics, Security & Resiliency (91–100)
91. **Encrypted Credentials Store:** Save credentials (PhotoPrism tokens, WebDAV passwords, Wi-Fi keys) inside secure keyrings.
92. **Empty Folder Notification Card:** Automatically display setup instructions when no pictures are found.
93. **Read-Only FS Compatibility:** Permit PicoGallery to write database indexes and caches strictly inside tmpfs RAM storage.
94. **Display Loop Health Check API:** Expose `/api/ping` endpoints to check if display rendering loops are frozen.
95. **Panic Handler Diagnostic Printer:** Intercept system panics and write backtrace data directly onto the DRM framebuffer.
96. **TLS Certificate Pinning Support:** Pin certificates for local WebDAV and PhotoPrism servers to prevent security warnings.
97. **Exponential Backoff Reconnect Patterns:** Backoff network connection attempts during router reboot cycles.
98. **Log Size Rotation Constraints:** Force local log parameters to stay below a strict storage threshold.
99. **Hardware Temperature Automatic Safe-Shutdown:** Safely shut down the system if critical board temperatures are reached.
100. **Dry-Run Validation Command:** Introduce `picogallery --dry-run` to verify directories, config parameters, and display routes.
