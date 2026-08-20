# PhotoPrism API Review: Best Features for Rust Plugins

This review analyzes the **PhotoPrism REST API** (queried from `docs.photoprism.app` and Swagger documentation) and identifies the best features to implement as a **Rust plugin** (e.g., for a digital photo frame or gallery client running on a Raspberry Pi Zero 2 W).

---

## 🔑 Core API Mechanism: Cookie-Free Auth & Streaming
Unlike typical enterprise web APIs that rely on complex CORS cookies or heavy OAuth flows, PhotoPrism uses a **cookie-free, latency-optimized API** model.

Once a session is established (`POST /api/v1/session`), authentication is handled via:
- Standard headers: `Authorization: Bearer <token>` or `X-Auth-Token: <token>`.
- Inline tokens: Putting the security token directly in media URLs (essential for low-level media pipelines).

---

## 🌟 Top 5 PhotoPrism API Features for Plugins

### 1. Cookie-Free Thumbnail Server (`GET /api/v1/t/{hash}/{token}/{size}`)
*   **Description:** Fetch pre-generated, resized images directly by file hash.
*   **Why it is a must-have for a Rust/Pi Zero Plugin:**
    - **Resource Efficiency:** The Pi Zero 2 W has only 512MB RAM. If the plugin downloads a raw 48MB JPEG, decoding it in Rust will trigger an OOM crash. PhotoPrism pre-generates thumbnails of multiple sizes (e.g., `fit_720`, `fit_1280`, `fit_1920`). The plugin can request the exact size matching the screen resolution.
    - **Low Overhead:** Since it doesn't require a stateful TCP connection or headers, you can pass this URL straight to your image rendering engine or media loader.
    - **Fast Rendering:** Rust can decode pre-scaled `fit` JPEGs in a fraction of a millisecond.

### 2. Rich Query Syntax Searching (`GET /api/v1/photos?q=...&count=...`)
*   **Description:** Retrieve list of photo files based on sophisticated queries.
*   **Why it is a must-have for a Rust/Pi Zero Plugin:**
    - PhotoPrism supports search parameters like `q=favorites` (starred photos), `q=nature` (category detection), `q=country:us`, or date ranges `q=2024-06`.
    - **Smart Playlists:** A Rust plugin can expose a configuration option (e.g., `search_query: String`) on the picture frame. The plugin will query PhotoPrism, get a fresh list of SHA1 hashes, and play those photos.
    - **Pagination:** Utilizing `count` and `offset` allows the plugin to load metadata incrementally in the background without hogging memory.

### 3. Album Detail Extraction (`GET /api/v1/albums/{uid}`)
*   **Description:** Get metadata and photo listings for a specific album by UID.
*   **Why it is a must-have for a Rust/Pi Zero Plugin:**
    - Users prefer choosing specific albums to display on their frame (e.g., "Family Vacation 2025").
    - The plugin can query this endpoint to populate the slideshow playlist. It returns a clean JSON list containing the hashes of all photos in that album.

### 4. Remote Index/Import Triggers (`POST /api/v1/index` / `POST /api/v1/import`)
*   **Description:** Remotely trigger PhotoPrism to index new photos in its originals directory.
*   **Why it is a must-have for a Rust/Pi Zero Plugin:**
    - If the Raspberry Pi Zero 2 has a physical USB port or SD card reader attached, the plugin can detect when a user plugs in a USB, copy files to a shared NAS folder, and trigger a PhotoPrism index update automatically.
    - Enables bidirectional interaction: the frame isn't just a screen; it's a doorway to ingest media.

### 5. Server Configuration & Features Info (`GET /api/v1/config`)
*   **Description:** Retrieve current settings and enabled features of the PhotoPrism server.
*   **Why it is a must-have for a Rust/Pi Zero Plugin:**
    - Dynamic capability adjustments. The plugin can determine if WebDAV is enabled, if download limits are active, or if certain folders are read-only.
    - This config payload allows the plugin to adjust its rendering styles or enable/disable upload capabilities.

---

## 🛠️ Rust Implementation Architecture for a PhotoPrism Plugin

If you are developing a Rust plugin for a local client to pull photos from PhotoPrism, use this architecture:

```mermaid
graph TD
    A[Pico-Gallery Rust Plugin] -->|1. POST /api/v1/session| B(PhotoPrism Server)
    B -->|Returns Access Token| A
    A -->|2. GET /api/v1/albums/{uid}| B
    B -->|Returns SHA1 Hashes| A
    A -->|3. GET /api/v1/t/{hash}/{token}/fit_1280| B
    B -->|Returns Binary JPEG Buffer| A
    A -->|4. Decode & Render| C[Linux KMS/DRM Framebuffer]
```

### Proposed Crate Stack for the Rust Plugin:
1. **`reqwest` + `tokio`:** For making asynchronous, concurrent HTTP requests.
2. **`serde` + `serde_json`:** For parsing PhotoPrism's search and configuration JSON payloads.
3. **`image`:** To quickly decode the fetched `fit_xxx` JPEGs.
4. **`lru` / Caching crate:** Store a local LRU (Least Recently Used) cache of downloaded thumbnails on the SD card so that the frame can continue playing offline if PhotoPrism goes offline.

### ⚠️ Performance Guardrails (Vetted for Pi Zero 2 W)
- **Do not use `/api/v1/albums/{uid}/dl` (ZIP download):** Downloading and unzipping a large archive on the Pi Zero 2 W will max out its CPU and easily exceed the 512MB memory boundary during extraction. Always stream images individually.
- **Use `merged=true` on search queries:** This groups files of the same photo (e.g. RAW + JPEG) together, ensuring you only request and display the processed JPEG thumbnail rather than attempting to decode a camera RAW file.
- **Precompute the next image in the background:** While the current photo is displaying, use a background `tokio` task to fetch the next photo's thumbnail into a memory buffer. This ensures lag-free slide transitions.
