# Implementation Plan: PhotoPrism SQL & NFS Integration (Enriched)

This plan outlines the architecture and changes required to implement **Option 1 (Direct SQL Connection + NFS Mount)**, connecting a Raspberry Pi Zero 2 W client (running PicoGallery) directly to a Raspberry Pi 4 server (running PhotoPrism) over a local network.

---

## 1. End-to-End Architecture

This architecture bypasses the HTTP REST API layer of PhotoPrism. The Pi Zero 2 W acts as a native database client and mounts the Pi 4 storage directories directly.

```
┌─────────────────────────────────────────────────────────────┐
│                   Raspberry Pi 4 (Server)                   │
│                                                             │
│  ┌──────────────────────┐        ┌───────────────────────┐  │
│  │   MariaDB Database   │        │     Originals /       │  │
│  │     (Port 3306)      │        │  Cache Folders (NFS)  │  │
│  └──────────┬───────────┘        └───────────┬───────────┘  │
└─────────────┼────────────────────────────────┼──────────────┘
              │ TCP (SQL Queries)              │ NFS/TCP (Port 2049)
              │ (Keep-Alives Enabled)          │ (Read-Only Mount)
┌─────────────┼────────────────────────────────┼──────────────┐
│             ▼                                ▼              │
│  ┌──────────────────────┐        ┌───────────────────────┐  │
│  │   MariaDB Client     │        │     Mountpoint        │  │
│  │    (sqlx Pool)       │        │  /mnt/photoprism/     │  │
│  └──────────┬───────────┘        └───────────┬───────────┘  │
│             │                                │              │
│             └────────────────┬───────────────┘              │
│                              ▼                              │
│                  ┌──────────────────────┐                   │
│                  │   PicoGallery App    │                   │
│                  └──────────────────────┘                   │
│                                                             │
│                Raspberry Pi Zero 2 W (Client)               │
└─────────────────────────────────────────────────────────────┘
```

---

## 2. Infrastructure Setup (Pi 4 & Pi Zero 2 W)

### A. Pi 4 Server Configuration (NFS & MariaDB)
1. **NFS Exports:** Export the originals directory and storage/cache directories in `/etc/exports`:
   ```exports
   /var/lib/photoprism/originals  192.168.1.0/24(ro,sync,no_subtree_check,all_squash,anonuid=1000,anongid=1000)
   /var/lib/photoprism/storage    192.168.1.0/24(ro,sync,no_subtree_check,all_squash,anonuid=1000,anongid=1000)
   ```
2. **Database Permissions:** Allow the client user to connect from the LAN subnet. Run on MariaDB:
   ```sql
   CREATE USER 'picogallery'@'192.168.1.%' IDENTIFIED BY 'secure_password';
   GRANT SELECT, UPDATE ON photoprism.* TO 'picogallery'@'192.168.1.%';
   FLUSH PRIVILEGES;
   ```

### B. Pi Zero 2 W Client Configuration
1. **Mount Points:** Mount the NFS folders using `autofs` to handle network disconnects cleanly:
   ```conf
   # /etc/auto.photoprism
   originals  -ro,soft,intr,tcp,timeo=50,retrans=3  192.168.1.4:/var/lib/photoprism/originals
   storage    -ro,soft,intr,tcp,timeo=50,retrans=3  192.168.1.4:/var/lib/photoprism/storage
   ```
2. **Mount Verification File:** Create a signature file `/var/lib/photoprism/originals/.mounted_by_picogallery` on the server so the client can verify the mount is active before reading.

---

## 3. PicoGallery Code Modifications

### A. Cargo Workspace Additions
Add SQL dependency support inside `Cargo.toml`. We will use `sqlx` with native TLS for async MySQL/MariaDB connections:
```toml
# Cargo.toml workspace dependencies
sqlx = { version = "0.7", features = ["runtime-tokio", "mysql", "tls-rustls"] }
```

Create a new crate `plugins/photoprism-sql` with the following structure:
* `plugins/photoprism-sql/Cargo.toml`
* `plugins/photoprism-sql/src/lib.rs`

### B. Configuration Structure ([config.rs](../src/config.rs))
Introduce a new plugin configuration:
```toml
[[plugins]]
name = "photoprism-sql"
enabled = true
db_url = "mysql://picogallery:secure_password@192.168.1.4/photoprism"
mount_originals = "/mnt/photoprism/originals"
mount_storage = "/mnt/photoprism/storage"
# Query filters
favorites = true
quality = 3
```

### C. Database Query Logic & Schema Mapping
The plugin will directly query PhotoPrism's database tables:

1. **Listing Photos:**
   ```sql
   SELECT p.id, p.photo_uid, p.photo_name, p.photo_title, p.photo_taken_at,
          f.file_name, f.file_hash
   FROM photos p
   JOIN files f ON f.photo_id = p.id
   WHERE p.photo_private = 0
     AND p.photo_quality >= ?
     AND f.file_primary = 1
     AND f.file_missing = 0
   ORDER BY p.photo_taken_at DESC
   LIMIT ? OFFSET ?;
   ```
2. **Accessing Cached Thumbnails:**
   PhotoPrism generates thumbnails inside the storage folder: `storage/cache/thumbnails/{hash_prefix}/{hash_suffix}/{size}.jpg`. The client will construct this path directly using the `file_hash` retrieved from the database query and read the image from `/mnt/photoprism/storage/cache/thumbnails/...`.
3. **Updating Favorites:**
   ```sql
   UPDATE photos SET photo_favorite = ? WHERE photo_uid = ?;
   ```

---

## 4. Internal Architecture Details

### A. Connection Pool Management (sqlx::MySqlPool)
On resource-constrained hardware like the Pi Zero 2 W, we must avoid leaking connections or exhausting file descriptors. The connection pool must be restricted:
* **Max Connections:** Limit to `2` concurrent connections (one for metadata retrieval, one for live updates like setting favorites).
* **Min Connections:** Set to `0` to release database resources when the slideshow is running off local cached data.
* **Idle Timeout:** Close idle connections after `60` seconds.
* **Acquire Timeout:** Set to `5` seconds to prevent blocking threads indefinitely during network drops.

```rust
// Crate implementation inside plugins/photoprism-sql/src/lib.rs
let pool = MySqlPoolOptions::new()
    .max_connections(2)
    .min_connections(0)
    .idle_timeout(Duration::from_secs(60))
    .acquire_timeout(Duration::from_secs(5))
    .connect(&config.db_url)
    .await?;
```

### B. NFS Mount Validation & Safety Checks
To prevent reading from or writing to empty mount directories (if the NFS share is unmounted or dropping), check for the presence of the signature file before initiating photo reads:
```rust
fn verify_nfs_mounts(mount_path: &Path) -> Result<()> {
    let signature_file = mount_path.join(".mounted_by_picogallery");
    if !signature_file.exists() {
        return Err(anyhow::anyhow!("NFS mount point is inactive. File not found: {:?}", signature_file));
    }
    Ok(())
}
```

### C. Advanced SQL Filter Mapping
PhotoPrism properties configured in `config.toml` map to SQL queries as follows:
* **Favorites Only (`favorites = true`):** Append `AND p.photo_favorite = 1` to query.
* **Specific Album (`album = "vacation"`):** Join with `photos_albums` and `albums` tables:
  ```sql
  JOIN photos_albums pa ON pa.photo_uid = p.photo_uid
  JOIN albums a ON a.album_uid = pa.album_uid
  WHERE a.album_slug = ?
  ```
* **Memories Mode (`memories = true`):**
  ```sql
  WHERE MONTH(p.photo_taken_at) = MONTH(CURRENT_DATE())
    AND DAY(p.photo_taken_at) = DAY(CURRENT_DATE())
  ```

---

## 5. Verification & Resiliency Plan

* **Network Dropout Handling:** Ensure the SQL pool is configured with a low connection timeout and dynamic reconnect limits.
* **NFS IO Locks:** Wrap standard file read operations in `tokio::fs::read` with a timeout guard to prevent the slideshow thread from hanging if the NFS mount becomes unresponsive.
