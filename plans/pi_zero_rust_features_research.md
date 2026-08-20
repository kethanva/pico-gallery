# 120 Advanced Photo Display Features for Raspberry Pi Zero 2 W (Rust Implementation)

Following an expanded deep-dive into open-source digital signage, MagicMirror modules, and advanced self-hosted photo galleries (like Immich, PhotoPrism, Screenly, and Pi3D), here is a comprehensive list of 120 features.

These features are categorized and specifically vetted for implementation using **Rust** on the **Raspberry Pi Zero 2 W**, ensuring they respect its 512MB RAM and Quad-core Cortex-A53 constraints.

---

## 🖼️ 1. Display & Graphics Engine (1-15)
1. **Direct DRM/KMS Rendering:** Bypass X11/Wayland entirely to write directly to the Linux Framebuffer, saving massive amounts of RAM.
2. **Zero-copy Buffer Management:** Pass decoded image memory directly to the GPU without CPU duplication.
3. **On-the-fly Image Downscaling:** Prevent OOM crashes by downscaling 4K images to 1080p during the decode process.
4. **E-Ink SPI Support:** Render to ultra-low power waveshare e-Paper displays for a "printed photo" look.
5. **Dynamic Refresh Rate (VRR):** Drop monitor refresh rate to 1Hz when viewing a static image to save power.
6. **Custom OpenGL ES Shaders:** Utilize the Pi's Videocore GPU for custom visual effects.
7. **Hardware Vsync:** Ensure zero screen tearing when transitioning between images.
8. **Alpha Blended Crossfades:** Smooth, GPU-accelerated fading between slides.
9. **Slide-in / Push Transitions:** Animated slide changes.
10. **Ken Burns Effect:** Slow, calculated panning and zooming across static images to add dynamic motion.
11. **CRT Retro Filter Overlay:** Add scanlines for a retro aesthetic.
12. **Color Space Conversion:** Handle YUV to RGB conversion efficiently on the GPU.
13. **Dual-Display Support:** Drive two small SPI LCDs simultaneously.
14. **Display Blanking (DPMS):** Physically cut HDMI output power.
15. **HDR to SDR Tone Mapping:** Compress High Dynamic Range iPhone photos to look correct on standard SDR displays.

## 📁 2. File System & Local Storage (16-25)
16. **Inotify Directory Watching:** Instantly load new photos dropped into the SD card without polling.
17. **USB Auto-mount & Sync:** Automatically copy photos from a plugged-in USB thumb drive.
18. **SD Card Wear Leveling:** Write application logs to a RAM disk (tmpfs) to extend SD card life.
19. **Configurable Root Directory:** Support multiple libraries on different partitions.
20. **Hidden Folder Exclusion:** Automatically ignore `.folders` and macOS `__MACOSX` directories.
21. **Symlink Following:** Support symbolic links for complex album organization without duplicating files.
22. **File Deduplication:** Fast MD5/SHA hashing to skip displaying exact duplicate images.
23. **Corrupted File Detection:** Catch and skip broken JPEGs gracefully without crashing the app.
24. **Read-only Root Filesystem:** Support running the OS in read-only mode to prevent corruption on sudden power loss.
25. **Trash/Recycle Bin:** Soft-delete photos instead of permanently removing them via the UI.

## ☁️ 3. Cloud & Network Sync (26-35)
26. **WebDAV Sync:** Background sync from Nextcloud or ownCloud.
27. **Google Photos API:** Authenticate and pull from specific Google Photos albums.
28. **SMB/CIFS Network Share:** Stream photos directly from a local NAS.
29. **rsync/Syncthing Integration:** Keep a local folder perfectly mirrored with a remote server.
30. **Offline-first Fault Tolerance:** Play cached photos seamlessly when Wi-Fi drops.
31. **Resumable Background Downloads:** Don't restart large downloads if the network blips.
32. **Wi-Fi Auto-Reconnect:** Rust daemon to monitor and repair Wi-Fi connections.
33. **Bandwidth Limiting:** Trickle-download photos so it doesn't lag the home network.
34. **S3 Bucket Pulling:** Fetch images securely from AWS S3 or Cloudflare R2.
35. **mDNS/Bonjour Discovery:** Broadcast `raspberrypi.local` on the network for easy admin access.

## 🎞️ 4. Image & Media Decoding (36-45)
36. **Hardware JPEG Decoding:** Use the Pi's OpenMAX/V4L2 hardware decoder for massive speedups.
37. **HEIC/HEIF Support:** Decode modern iPhone photos natively.
38. **WebP Support:** Support highly compressed modern web images.
39. **RAW Image Support:** Basic decoding of CR2, NEF, and ARW photographer formats.
40. **Animated GIF Support:** Play low-res looping GIFs.
41. **MP4 / H.264 Looping:** Hardware-accelerated video playback for short "Live Photos".
42. **Video Muting:** Ensure videos play silently so the frame isn't disruptive.
43. **Audio Track Playback:** Option to play MP3 ambient sound for specific albums.
44. **SVG Rendering:** Render scalable vector graphics for overlays.
45. **Automatic Color Profile (ICC) Correction:** Ensure colors match the photographer's intent.

## 🏷️ 5. Metadata Handling (46-55)
46. **EXIF Orientation Auto-Rotation:** Prevent upside-down photos.
47. **GPS Coordinate Extraction:** Read EXIF Lat/Lon data.
48. **Reverse Geocoding:** Convert GPS coordinates to City/Country names using an offline database or API.
49. **IPTC Title/Description Parsing:** Read embedded captions from Lightroom/Photoshop.
50. **EXIF Date Original:** Sort and display based on when the photo was taken, not file creation time.
51. **Camera Model & Lens Display:** Show "Shot on Sony A7III" in the corner.
52. **Face Bounding Boxes:** Use metadata to ensure faces aren't cropped out.
53. **XMP Sidecar Support:** Read metadata from external `.xmp` files.
54. **Tag Filtering:** Only show photos containing specific embedded tags.
55. **Rating Filtering:** Only show photos rated 4 stars or higher.

## 🧠 6. Smart Playback & Scheduling (56-70)
56. **Fisher-Yates Smart Shuffle:** Guarantee every photo plays exactly once before any repeat.
57. **Weighted Random:** Show photos added in the last 30 days twice as often as old ones.
58. **Chronological Playback:** Walk through a timeline of memories.
59. **Time-of-Day Context:** Show dark/sunset photos at night, bright photos in the morning.
60. **"On This Day":** Prioritize photos taken on today's date in previous years.
61. **Configurable Slide Speed:** Allow 5 seconds to 24 hours per image.
62. **Pause on Face Detection:** Stop slideshow if a camera module detects someone looking at the frame.
63. **Event Triggers:** Auto-switch to the "Birthdays" album on a specific date.
64. **Cron-based Sleep/Wake:** E.g., `0 23 * * *` sleep, `0 7 * * *` wake.
65. **Holiday Theme Overrides:** Add subtle snow overlays in December.
66. **Playback History Logging:** Keep track of exactly what was shown.
67. **Skip Button:** Physical or digital button to skip the current photo.
68. **Previous Button:** Go back if you missed a photo.
69. **Hold/Pause:** Freeze on a photo indefinitely.
70. **M3U Playlist Importing:** Support standard playlist files for image sequences.

## 🔌 7. Hardware & Sensors (71-85)
71. **PIR Motion Sensor Wake:** Wake screen when someone walks by (GPIO).
72. **Microwave Radar (RCWL-0516):** Detect motion through wood/plastic bezels.
73. **Ambient Light Sensor (I2C):** Auto-dim the backlight in a dark room.
74. **PWM Backlight Dimming:** Smoothly transition brightness via hardware PWM.
75. **Physical Push Buttons:** Wire arcade buttons to GPIO for Next/Prev.
76. **Rotary Encoder:** Scroll back and forth through time with a dial.
77. **BME280 Environment Sensor:** Read and display room temp/humidity.
78. **CPU Throttling Overlay:** Show a tiny thermometer icon if the Pi is overheating.
79. **GPIO Fan Control:** Turn on a cooling fan only when CPU hits 65°C.
80. **Pi Camera Integration:** Take a photo of the room and sync it.
81. **NeoPixel (WS2812B) Ambilight:** Glow LEDs behind the frame matching the photo's edge colors.
82. **RTC (Real Time Clock):** Keep time accurately without Wi-Fi.
83. **Capacitive Touch Screen:** Support swiping gestures on touch displays.
84. **IR Remote Control:** Use a standard TV remote (LIRC) to control the frame.
85. **Status LED Toggling:** Disable the Pi's glaring green activity LED via sysfs.

## 🎨 8. Overlays & UI (86-100)
86. **Analog/Digital Clock:** Minimalist time display in the corner.
87. **Live Weather Forecast:** Pull from OpenWeatherMap API.
88. **Location Name Overlay:** Display the reverse-geocoded city.
89. **Progress Bar:** A tiny line at the bottom showing time remaining on slide.
90. **"New Photo" Badge:** Highlight newly added photos for 24 hours.
91. **QR Code Overlay:** Generate a QR code so guests can scan and download the photo to their phone.
92. **Custom TTF/OTF Fonts:** Load premium typography.
93. **Text Shadow/Outline:** Ensure white text is readable on snow backgrounds.
94. **Dynamic Font Scaling:** Adjust text size based on screen resolution.
95. **Minimalist Mode:** Hide all overlays.
96. **Photo Collage Layout:** Stitch 4 portrait photos together to fill a landscape screen.
97. **Blurry Backgrounds (Pillarbox):** Instead of black bars on vertical photos, show a zoomed, blurred version of the photo as the background.
98. **Smart Cropping:** Center-crop to fill the screen vs. Fit inside screen.
99. **Grid View UI:** A thumbnail browser mode.
100. **Pixel Shifting:** Slowly move static overlays 1 pixel per minute to prevent OLED/LCD burn-in.

## 📱 9. Admin Web Dashboard (101-110)
101. **Embedded Rust Web Server:** Extremely fast, low-memory Axum server.
102. **PWA Mobile Interface:** Looks like a native app on iOS/Android.
103. **Drag & Drop Upload:** Push photos straight from phone to frame.
104. **Delete Button in UI:** Trash a photo remotely.
105. **Live Screen Preview:** See exactly what the frame is displaying on your phone.
106. **System Resource Monitor:** Web dashboard showing RAM, CPU load, and Temperature.
107. **Album Management:** Create, rename, and organize folders via web.
108. **WebSocket Logs Viewer:** Watch the application logs stream live in the browser.
109. **Remote Power Controls:** Reboot or shutdown the Pi gracefully from the web.
110. **TOML Configuration Editor:** Edit backend settings safely from the UI.

## 🌐 10. IoT & System Integrations (111-120)
111. **MQTT Client:** Publish state (current photo, on/off) and subscribe to commands.
112. **Home Assistant Auto-Discovery:** Seamlessly appear in Home Assistant as a Media Player entity.
113. **RESTful API Endpoints:** Standard JSON API (`/api/v1/next`).
114. **Telegram Bot Integration:** Send a photo to a Telegram bot, and it appears on the frame instantly.
115. **Slack Webhook:** Post frame updates to a Slack channel.
116. **Systemd Kiosk Auto-Start:** Robust background daemon management.
117. **OTA Binary Updates:** Download statically linked ARM64 updates directly from GitHub Releases.
118. **Watchdog Timer:** Hardware watchdog to auto-reboot if the Rust thread panics.
119. **Local API Authentication:** Bearer tokens for secure API access.
120. **Prometheus Metrics:** Export performance data for Grafana monitoring.

---
*These 120 features represent the absolute state-of-the-art in DIY Digital Signage and Photo Galleries. By leveraging Rust's safety and memory efficiency, all of this can run comfortably on a $15 Raspberry Pi Zero 2 W.*
