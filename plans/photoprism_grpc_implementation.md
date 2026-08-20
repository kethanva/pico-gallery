# Implementation Plan: PhotoPrism gRPC Sidecar Integration (Enriched)

This plan outlines the architecture and changes required to implement **Option 2 (gRPC Sidecar Proxy)**, connecting a Raspberry Pi Zero 2 W client (running PicoGallery) to a Raspberry Pi 4 server (running PhotoPrism) via a custom binary protocol.

---

## 1. End-to-End Architecture

This architecture introduces a lightweight Go-based daemon running on the Pi 4 (co-located with PhotoPrism). The daemon exposes a gRPC port, translates binary RPC requests into queries, fetches files from the local filesystem on the Pi 4, and streams photo bytes back to the Pi Zero 2 W in binary chunks.

```
┌─────────────────────────────────────────────────────────────┐
│                   Raspberry Pi 4 (Server)                   │
│                                                             │
│  ┌──────────────────────┐        ┌───────────────────────┐  │
│  │      PhotoPrism      │        │     Go gRPC Proxy     │  │
│  │    (Local Host)      │ ◀────▶ │       (Sidecar)       │  │
│  └──────────────────────┘        └───────────▲───────────┘  │
└──────────────────────────────────────────────┼──────────────┘
                                               │ gRPC over HTTP/2
                                               │ tcp/50051 (TLS Optional)
┌──────────────────────────────────────────────┼──────────────┐
│                                              ▼              │
│                  ┌───────────────────────────────────────┐  │
│                  │            PicoGallery App            │  │
│                  │       (picogallery-grpc plugin)       │  │
│                  └───────────────────────────────────────┘  │
│                                                             │
│                Raspberry Pi Zero 2 W (Client)               │
└─────────────────────────────────────────────────────────────┘
```

---

## 2. Protobuf Service Definition

Create a file `proto/photoprism.proto` defining the service interface:

```protobuf
syntax = "proto3";
package photoprism.v1;

service PhotoPrismService {
  rpc ListPhotos (ListPhotosRequest) returns (ListPhotosResponse);
  rpc GetPhoto (GetPhotoRequest) returns (stream GetPhotoResponse);
  rpc SetFavorite (SetFavoriteRequest) returns (SetFavoriteResponse);
}

message ListPhotosRequest {
  uint32 limit = 1;
  uint32 offset = 2;
  string query = 3;
  bool favorites_only = 4;
}

message PhotoMetadata {
  string id = 1;
  string filename = 2;
  uint32 width = 3;
  uint32 height = 4;
  string taken_at = 5;
}

message ListPhotosResponse {
  repeated PhotoMetadata photos = 1;
}

message GetPhotoRequest {
  string photo_id = 1;
  uint32 target_width = 2;
  uint32 target_height = 3;
}

message GetPhotoResponse {
  bytes chunk = 1;
}

message SetFavoriteRequest {
  string photo_id = 1;
  bool favorite = 2;
}

message SetFavoriteResponse {
  bool success = 1;
}
```

---

## 3. Go Sidecar Implementation (Pi 4 Server)

Implement a Go backend (leveraging Go's gRPC framework) that runs on the Pi 4:
1. **API Integration:** Connects to the local PhotoPrism SQLite/MariaDB database or uses Go HTTP calls to `/api/v1` on localhost.
2. **Metadata Resolver:** Implements `ListPhotos` by querying the DB and mapping the structures to the Protobuf models.
3. **Chunked Streaming:** Implements `GetPhoto` by reading thumbnail files from `/var/lib/photoprism/storage/cache/...` and writing them to the gRPC response stream in 64 KB chunks.

---

## 4. PicoGallery Changes (Pi Zero 2 W Client)

### A. Cargo Dependencies
Add `tonic` and `prost` to compile Protobuf files and handle async gRPC traffic:
```toml
[dependencies]
tonic = { version = "0.10", features = ["transport"] }
prost = "0.12"

[build-dependencies]
tonic-build = "0.10"
```

Configure `build.rs` to compile the proto file at build time:
```rust
fn main() {
    tonic_build::compile_protos("proto/photoprism.proto").unwrap();
}
```

### B. Plugin Implementation
Create a new plugin crate `plugins/grpc` containing:
1. **Connection Pool:** Initialize a connection channel to `http://192.168.1.4:50051`.
2. **`list_photos` implementation:** Make the async gRPC call `ListPhotos` and convert returned Protobuf structs into `PhotoMeta` structs.
3. **`get_photo_bytes` implementation:** Call `GetPhoto`, read chunks from the stream, append them to a `Vec<u8>` buffer, and return the complete image payload.

---

## 5. Internal Architecture Details

### A. Client Transport Keepalives (`tonic::transport::Channel`)
On low-performance networks, inactive TCP connections can be closed silently. Configure Tonic with strict keepalive parameters to keep the channel open and detect drops early:
* **Keepalive Time:** Send HTTP/2 pings every `30` seconds if there is no active traffic.
* **Keepalive Timeout:** Close the connection if the ping response isn't received within `5` seconds.
* **Keepalive While Idle:** Enable keepalives even when no requests are active to preserve session state.

```rust
// Connection configuration in plugins/grpc/src/lib.rs
let channel = Channel::from_static("http://192.168.1.4:50051")
    .keep_alive_while_idle(true)
    .http2_keep_alive_interval(Duration::from_secs(30))
    .keep_alive_timeout(Duration::from_secs(5))
    .connect()
    .await?;
```

### B. Pre-shared Token Authentication (gRPC Interceptor)
Since the gRPC server is open on the LAN, add a simple authorization header guard to prevent unauthorized client requests:
```rust
// Client interceptor inserting metadata credentials
let client = PhotoPrismServiceClient::with_interceptor(channel, move |mut req: Request<()>| {
    req.metadata_mut().insert(
        "authorization",
        MetadataValue::from_str("Bearer client_pre_shared_secret_token").unwrap(),
    );
    Ok(req)
});
```

### C. Memory-Efficient Streaming on Pi Zero 2 W
A 1080p JPEG image is ~2–8 MB. Standard memory allocations can trigger memory spikes. We stream chunks in fixed `32 KB` or `64 KB` limits:
* **Allocation Tuning:** Pre-allocate the result vector capacity `Vec::with_capacity(expected_size)` to prevent multiple vector resizing reallocations.
* **Streaming directly to decoder:** Utilize the chunk stream reader to append directly to a circular file ring or pipe straight to the JPEG decoder (`zune-jpeg` or `image` crate decoder interfaces).

---

## 6. Verification & Performance Tuning

* **Buffering & Throughput:** Test chunk size thresholds (e.g. 32 KB vs 64 KB vs 128 KB) to optimize transmission speeds over Wi-Fi.
* **TCP Keepalives:** Implement keepalive pings in the `tonic` channel configuration to prevent the connection from being dropped by routers or firewall rules.
