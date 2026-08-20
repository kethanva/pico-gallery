# Implementation Plan: Split Client-Server Architecture for PicoGallery

This plan details the implementation of a robust **Split Client-Server Architecture** for PicoGallery. The heavy backend logic (directory scanning, third-party plugin syncing, database caching, web remote admin, and image resizing) runs on a **Raspberry Pi 4 (Server)**, while a lightweight rendering loop runs on a **Raspberry Pi Zero 2 W (Client)**.

---

## 1. End-to-End System Architecture

```
┌────────────────────────────────────────────────────────────────────────┐
│                        Raspberry Pi 4 (Server)                         │
│                                                                        │
│  ┌──────────────────┐    ┌─────────────────┐    ┌───────────────────┐  │
│  │  Photo Sources   │ ──▶│  Image Resizer  │ ──▶│   axum HTTP API   │  │
│  │ (Directory, etc) │    │  (SIMD-backed)  │    │    (Port 8188)    │  │
│  └──────────────────┘    └────────┬────────┘    └─────────▲─────────┘  │
│                                   ▼                       │            │
│                          ┌─────────────────┐              │            │
│                          │  Resized Cache  │ ─────────────┘            │
│                          │ (Server Disk)   │                           │
│                          └─────────────────┘                           │
└────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ HTTP / LAN (Keep-Alives)
                                    │ GET /api/v1/next?w=1920&h=1080
                                    │
┌───────────────────────────────────┼────────────────────────────────────┐
│                                   ▼                                    │
│  ┌──────────────────────────────────────────────────────────────────┐  │
│  │                   reqwest Async Client Connection                │  │
│  └────────────────────────────────┬─────────────────────────────────┘  │
│                                   ▼                                    │
│  ┌──────────────────────────────────────────────────────────────────┐  │
│  │                   Double-Buffer Render Queue                     │  │
│  │                [Active Texture] ◀── [Back Buffer]                │  │
│  └────────────────────────────────┬─────────────────────────────────┘  │
│                                   ▼                                    │
│  ┌──────────────────────────────────────────────────────────────────┐  │
│  │                   KMS/DRM Framebuffer (SDL2)                     │  │
│  └──────────────────────────────────────────────────────────────────┘  │
│                                                                        │
│                     Raspberry Pi Zero 2 W (Client)                     │
└────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Server Architecture Details (Raspberry Pi 4)

The server runs a daemon built on the Axum HTTP framework.

### A. API Endpoint Specifications
1. **Get Next Photo (`GET /api/v1/next`)**
   * **Parameters:**
     * `w`: Target display width in pixels (e.g. `1920`).
     * `h`: Target display height in pixels (e.g. `1080`).
   * **Response:** Raw JPEG byte stream (`image/jpeg`).
   * **Behavior:**
     * Fetches the metadata of the next photo in the active playlist/queue.
     * Checks if a pre-scaled JPEG matches the target width/height in the server's cache.
     * If not cached, runs the resizing pipeline, saves to disk, and serves the bytes.
2. **Slideshow Status (`GET /api/v1/status`)**
   * **Response:** JSON metadata of the currently showing image (filename, taken date, album).
3. **Remote Control Actions (`POST /api/v1/control`)**
   * **Payload:** `{ "action": "next" | "prev" | "pause" | "favorite" }`
   * **Response:** Status code `200 OK`.

### B. SIMD-Backed Resizing Pipeline
To maximize Pi 4 performance:
* Use the `fast_image_resize` crate with CPU feature detection (ARM NEON/SIMD enabled).
* Scale using Lanczos3 filtering for premium sharpness on large screens.
* Pre-scaled cache folder structure:
  ```
  /var/lib/picogallery/resized/{width}x{height}/{file_hash}.jpg
  ```

---

## 3. Client Architecture Details (Raspberry Pi Zero 2 W)

The client is a minimalist Rust binary compiled without local/cloud sync features.

### A. Memory-Efficient Double-Buffering
To prevent frame stutter and OOM panics on the Pi Zero 2 W's 512MB RAM:
1. Maintain exactly two image buffers in RAM: the `ActiveBuffer` (rendering on screen) and the `BackBuffer` (currently prefetching).
2. The prefetch task runs asynchronously. It pulls the raw JPEG bytes from `/api/v1/next` directly into the `BackBuffer`'s vector.
3. During a slide transition, the JPEG bytes are decoded directly into an SDL2 texture, the `BackBuffer` swaps to `ActiveBuffer`, and the memory from the old slide is immediately released.

```rust
struct SlideshowClient {
    active_texture: Option<sdl2::render::Texture>,
    back_buffer: Vec<u8>, // holds raw JPEG bytes of the next slide
    client: reqwest::Client,
    server_url: String,
}

impl SlideshowClient {
    async fn prefetch_next(&mut self) -> Result<()> {
        let url = format!("{}/api/v1/next?w=1920&h=1080", self.server_url);
        let bytes = self.client.get(&url)
            .send()
            .await?
            .bytes()
            .await?;
        self.back_buffer = bytes.to_vec();
        Ok(())
    }
}
```

### B. Network Resiliency & Safe Offline Mode
To handle LAN drops (common on Wi-Fi digital frames):
1. **Client Cache:** Store the last 3 fetched JPEGs in a persistent RAM disk (`/dev/shm`) on the client.
2. **Error Interception:** If a prefetch call fails (network timeout or server reboot), the client:
   * Retries the endpoint using exponential backoff (e.g. 5s, 10s, 20s).
   * Falls back to cycling through the local RAM disk JPEGs in the meantime.
   * Overlays a subtle warning icon in the OSD layer to notify the user of disconnection.

---

## 4. Security & Connection Stability

* **Pre-shared API Token:** All client requests are authenticated using a custom token header:
  ```http
  X-Pico-Token: server_pre_shared_secret_token
  ```
* **Persistent Connections (HTTP Keep-Alives):** Configure the client's `reqwest` connection pool to keep sockets open, reducing TCP handshake overhead on low-power Wi-Fi:
  ```rust
  let client = reqwest::Client::builder()
      .tcp_keepalive(Some(Duration::from_secs(60)))
      .pool_max_idle_per_host(2)
      .build()?;
  ```

---

## 5. Client Resource Usage Profile (Raspberry Pi Zero 2 W)

Running strictly in **Frontend Client Mode** yields an exceptionally lightweight resource profile:

### A. Memory (RAM) Allocation
| Metric | Memory Allocation | Description |
| :--- | :--- | :--- |
| **PicoGallery Process (RSS)** | **~8 MB – 12 MB** | Core application state, network socket, and SDL2 library hooks. |
| **Decoded Framebuffers** | **~8.3 MB – 16.6 MB** | A 1080p (1920x1080) uncompressed RGBA pixel buffer takes 8.3 MB. During active transitions, two buffers are briefly held in memory. |
| **GPU Shared Memory (`gpu_mem`)** | **64 MB** | Pre-allocated in `/boot/config.txt` to hold the SDL2 hardware textures. |
| **Total System RAM Used** | **~40 MB – 60 MB** | Includes the minimal Linux OS (Raspberry Pi OS Lite) and PicoGallery. |

*With `gpu_mem` set to `64 MB`, the system has **448 MB** of system RAM. Utilizing only ~50 MB leaves a safe cushion of **~400 MB of free memory**, completely eliminating the risk of Out-of-Memory (OOM) crashes.*

### B. CPU Utilization (Quad-Core Cortex-A53)
*   **Static Slide View (Slide is displayed):** **< 1% CPU utilization** (across one core). The renderer drops to a low frame-rate cap or enters a sleep-state when not animating. The OSD clock updates only once per minute.
*   **Active Slide Transitions (e.g., 800ms Fade):** **~10% – 15%** of a single core (~3% total CPU). SDL2 draws textures using GLES hardware rendering via the GPU, leaving the CPU idle.
*   **JPEG Image Decoding:** Spikes to **~20%** of a single core for **100ms – 150ms** when a new slide is fetched and decoded by the highly-optimized `zune-jpeg` SIMD decoder.

### C. Disk I/O (SD Card Wear)
*   **Writes:** **0 bytes/second** (excluding minimal syslog output). All images are fetched directly into RAM over HTTP, meaning the SD card is never written to. This allows configuring the Pi Zero 2 W with a **Read-Only (RO) filesystem**, preventing SD card corruption when unplugged.
*   **Reads:** **0 bytes/second** after startup (the binary is fully resident in RAM).

### D. Network Bandwidth
*   **Per Slide:** **~200 KB – 500 KB** per transition (for a pre-scaled 1080p JPEG).
*   **Hourly Bandwidth (at 10s slide duration):** **~100 MB – 150 MB/hour** of local LAN traffic.

---

## 6. Fresh Raspberry Pi Zero 2 W Client Provisioning Steps

Follow these step-by-step instructions to configure a fresh, out-of-the-box Raspberry Pi Zero 2 W to boot directly into PicoGallery Frontend Client Mode:

### Step 1: Flash the OS
Using **Raspberry Pi Imager** on a computer:
1. Select **Raspberry Pi OS Lite (64-bit)** (no desktop environment).
2. Click the gear icon to pre-configure:
   * **Host Name:** `picoclient.local`
   * **SSH:** Enable SSH with key or password.
   * **Wi-Fi:** Input your local LAN credentials.
3. Flash to a micro SD card.

### Step 2: Configure GPU Memory Allocation
SSH into the client Pi and edit the boot configurations:
```bash
sudo nano /boot/firmware/config.txt
```
*(On older OS versions, edit `/boot/config.txt` instead)*. Append or modify this line:
```ini
gpu_mem=64
```

### Step 3: Install Runtime Dependencies
Install the rendering and display driver packages required by SDL2 and KMS:
```bash
sudo apt update
sudo apt install -y libsdl2-2.0-0 libdrm2 ca-certificates
```

### Step 4: Grant Permissions for direct Framebuffer access
Add the default user (e.g. `admin`) to the video, render, and input hardware groups to allow launching PicoGallery without root privileges:
```bash
sudo usermod -aG video,render,input $USER
```

### Step 5: Configure the Local Client settings
Create the configuration folder and save the client setup:
```bash
mkdir -p ~/.config/picogallery
nano ~/.config/picogallery/config.toml
```
Insert the following values (replace the IP with the Pi 4 Server IP):
```toml
[display]
slide_duration_secs = 10
transition = "fade"
width = 1920
height = 1080

[[plugins]]
name = "pico-server-client"
enabled = true
server_url = "http://192.168.1.4:8188"
```

### Step 6: Create the systemd Autostart Service
Create a systemd unit file to automatically boot and run the frame client:
```bash
sudo nano /etc/systemd/system/picogallery.service
```
Insert this service block:
```ini
[Unit]
Description=PicoGallery Frame Client
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=admin
ExecStart=/usr/local/bin/picogallery --config /home/admin/.config/picogallery/config.toml
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```
Enable the service:
```bash
sudo systemctl daemon-reload
sudo systemctl enable picogallery.service
```

### Step 7: Enable Read-Only (RO) Filesystem Protection
To make the frame robust against SD card corruption when powered off abruptly:
```bash
sudo raspi-config
```
1. Navigate to: **4 Performance Options**
2. Choose: **P3 Overlay File System**
3. Select **Yes** to enable the overlay file system (protecting the OS).
4. Select **Yes** to make the boot partition write-protected.
5. Exit and reboot. The Pi Zero 2 W is now fully provisioned and protected.
