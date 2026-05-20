# Graph Report - /Volumes/SSD/projects/pico-gallery  (2026-05-14)

## Corpus Check
- 33 files · ~337,288 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 398 nodes · 578 edges · 27 communities (24 shown, 3 thin omitted)
- Extraction: 91% EXTRACTED · 9% INFERRED · 0% AMBIGUOUS · INFERRED: 51 edges (avg confidence: 0.85)
- Token cost: 444,387 input · 190,450 output

## Community Hubs (Navigation)
- [[_COMMUNITY_Directory Plugin|Directory Plugin]]
- [[_COMMUNITY_Image Cache & Amazon Plugin|Image Cache & Amazon Plugin]]
- [[_COMMUNITY_WebDAV Plugin & Path Decoding|WebDAV Plugin & Path Decoding]]
- [[_COMMUNITY_Amazon Photos Auth|Amazon Photos Auth]]
- [[_COMMUNITY_EXIF & DRM Rendering|EXIF & DRM Rendering]]
- [[_COMMUNITY_Google Photos Plugin|Google Photos Plugin]]
- [[_COMMUNITY_Configuration System|Configuration System]]
- [[_COMMUNITY_Display Power & OSD Overlay|Display Power & OSD Overlay]]
- [[_COMMUNITY_Cache Entry Operations|Cache Entry Operations]]
- [[_COMMUNITY_Sample Tuscan Vineyard|Sample: Tuscan Vineyard]]
- [[_COMMUNITY_Sample Alpine Lake|Sample: Alpine Lake]]
- [[_COMMUNITY_Sample Enchanted Forest|Sample: Enchanted Forest]]
- [[_COMMUNITY_Sample Desert Dunes|Sample: Desert Dunes]]
- [[_COMMUNITY_Sample Aurora Cabin|Sample: Aurora Cabin]]
- [[_COMMUNITY_Plugin Trait Core|Plugin Trait Core]]
- [[_COMMUNITY_Main Entrypoint|Main Entrypoint]]
- [[_COMMUNITY_Sample Tropical Beach|Sample: Tropical Beach]]
- [[_COMMUNITY_Sample Autumn Forest|Sample: Autumn Forest]]
- [[_COMMUNITY_Sample Mount Fuji|Sample: Mount Fuji]]
- [[_COMMUNITY_Sample Lavender Field|Sample: Lavender Field]]
- [[_COMMUNITY_Sample Grand Canyon|Sample: Grand Canyon]]
- [[_COMMUNITY_Canyon Concept|Canyon Concept]]
- [[_COMMUNITY_Fuji Concept|Fuji Concept]]
- [[_COMMUNITY_Renderer SDL2 Build|Renderer SDL2 Build]]

## God Nodes (most connected - your core abstractions)
1. `WebDavPlugin` - 23 edges
2. `GooglePhotosPlugin` - 20 edges
3. `AmazonPhotosPlugin` - 19 edges
4. `DirectoryPlugin` - 16 edges
5. `LocalPlugin` - 12 edges
6. `Renderer` - 12 edges
7. `Slideshow::display_loop` - 12 edges
8. `ImageCache` - 10 edges
9. `Tuscan vineyard landscape at sunrise` - 10 edges
10. `PhotoPlugin trait` - 9 edges

## Surprising Connections (you probably didn't know these)
- `DRM probe` --rationale_for--> `KMS/DRM backend over X11`  [EXTRACTED]
  src/renderer.rs → architecture_docs/options/display-rendering.md
- `AmazonPhotosPlugin` --implements--> `PhotoPlugin trait`  [EXTRACTED]
  plugins/amazon-photos/src/lib.rs → core/src/lib.rs
- `build_plugins` --references--> `AmazonPhotosPlugin`  [EXTRACTED]
  src/main.rs → plugins/amazon-photos/src/lib.rs
- `build.rs (SDL Metal stub compile)` --conceptually_related_to--> `Renderer (SDL2)`  [INFERRED]
  build.rs → src/renderer.rs
- `DirectoryPlugin` --implements--> `PhotoPlugin trait`  [EXTRACTED]
  plugins/directory/src/lib.rs → core/src/lib.rs

## Hyperedges (group relationships)
- **Plugins implementing PhotoPlugin trait** — core_lib_photoplugin, directory_directoryplugin, local_localplugin, googlephotos_googlephotosplugin, amazonphotos_amazonphotosplugin, webdav_webdavplugin [EXTRACTED 1.00]
- **Display loop fetch-decode-render-cache pipeline** — slideshow_displayloop, slideshow_fetchphoto, cache_imagecache, renderer_decodeandscale, renderer_showfade, osd_drawphotoinfo [EXTRACTED 0.95]
- **Pi Zero memory-safety pipeline (gates + scale-then-rotate + single EXIF)** — renderer_decodeandscale, exifutil_readexif, exifutil_applyorientationrgba, config_displayconfig, rationale_scale_then_rotate [INFERRED 0.85]

## Communities (27 total, 3 thin omitted)

### Community 0 - "Directory Plugin"
Cohesion: 0.06
Nodes (16): auth_status_is_always_authenticated(), DirectoryPlugin, expand_home(), has_image_magic(), init_fails_for_nonexistent_directory(), init_fails_with_missing_path_key(), is_image(), list_photos_empty_before_init() (+8 more)

### Community 1 - "Image Cache & Amazon Plugin"
Cohesion: 0.05
Nodes (54): AmazonPhotosPlugin, ImageCache::evict_oldest, ImageCache::flush, ImageCache::get, ImageCache (LRU disk cache), ImageCache::put, CacheConfig, Config root (+46 more)

### Community 2 - "WebDAV Plugin & Path Decoding"
Cohesion: 0.08
Nodes (14): DavEntry, decode_relative_path(), decode_relative_path_normal(), filename_from_url(), from_hex_nibble(), is_image_content_type(), is_image_href(), is_image_magic() (+6 more)

### Community 3 - "Amazon Photos Auth"
Cohesion: 0.13
Nodes (8): AmazonPhotosPlugin, ContentProps, ImageInfo, LwaDeviceCode, LwaToken, Node, NodeList, StoredToken

### Community 4 - "EXIF & DRM Rendering"
Cohesion: 0.18
Nodes (9): apply_orientation_rgba(), ExifInfo, read_exif(), blit_centered(), DrmCard, probe(), Renderer, rgba_to_texture() (+1 more)

### Community 6 - "Configuration System"
Cohesion: 0.12
Nodes (11): CacheConfig, Config, default_cache_mb(), default_fps(), default_prefetch(), default_slide_duration(), default_transition_ms(), DisplayConfig (+3 more)

### Community 7 - "Display Power & OSD Overlay"
Cohesion: 0.19
Nodes (9): set_power(), darken_rect(), draw_char(), draw_glyph(), draw_photo_info(), draw_text(), truncate(), shuffle() (+1 more)

### Community 9 - "Sample: Tuscan Vineyard"
Cohesion: 0.22
Nodes (11): Aerial panoramic composition with leading line, Italian cypress trees lining hills, Soft sunrise sky with morning mist, Tuscan stone villas and farmhouses, Vineyard rows with grapevines, Wildflowers in foreground, Winding dirt path leading through vineyards, Tuscan vineyard landscape at sunrise (+3 more)

### Community 10 - "Sample: Alpine Lake"
Cohesion: 0.22
Nodes (10): Landscape nature photography, Symmetrical water reflection composition, Likely Moraine Lake or Banff style scenery, Serene and majestic mood, Mountain lake sunset landscape photo, Alpine glacial lake with turquoise water, Red canoe at wooden dock, Pine forest along shoreline (+2 more)

### Community 11 - "Sample: Enchanted Forest"
Cohesion: 0.27
Nodes (10): Enchanted Forest Waterfall Scene, Fantasy Digital Art Style, Ferns and Wildflowers Foreground, Glowing Fireflies and Bioluminescence, Magical Mystical Mood, Moss-Covered Ancient Trees, Nature Landscape Subject, Turquoise Reflective Pool (+2 more)

### Community 12 - "Sample: Desert Dunes"
Cohesion: 0.24
Nodes (10): Wide Landscape Composition with Leading Dune Ridge, Nature Landscape Photography, Slideshow Gallery Sample Photo, Desert Sunset Over Sand Dunes, Arid Desert Environment, Low Angle Backlighting Creating Long Shadows, Serene Warm Golden Hour Mood, Orange Amber and Deep Shadow Palette (+2 more)

### Community 13 - "Sample: Aurora Cabin"
Cohesion: 0.27
Nodes (10): Arctic Nightscape Photography, Aurora Borealis (Northern Lights), Illuminated Wooden Cabin, Frozen Lake with Reflection, Aurora Borealis Over Snowy Cabin, Mood: Magical, Serene, Remote, Distant Snowy Mountains, Snow-Dusted Pine Trees (+2 more)

### Community 14 - "Plugin Trait Core"
Cohesion: 0.29
Nodes (4): AuthStatus, PhotoMeta, PhotoPlugin, PluginConfig

### Community 15 - "Main Entrypoint"
Cohesion: 0.39
Nodes (6): Args, build_plugins(), default_config(), default_photo_dir(), generate_config(), main()

### Community 16 - "Sample: Tropical Beach"
Cohesion: 0.25
Nodes (8): Small figures of beachgoers and moored boat, Bright blue sky with scattered cumulus clouds, Forested islands on horizon, Coconut palm trees lining shoreline, Tranquil tropical paradise mood, Tropical beach with palm trees and turquoise water, Clear turquoise tropical sea, White sand beach curving along coastline

### Community 17 - "Sample: Autumn Forest"
Cohesion: 0.29
Nodes (8): Autumn forest path with vibrant fall foliage, Curving path as leading line into depth, Orange, red, and yellow autumn leaves, Fallen leaves carpeting forest floor, Dark vertical tree trunks framing path, Soft diffuse daylight through canopy, Serene, warm, contemplative autumn mood, Winding dirt trail through woodland

### Community 18 - "Sample: Mount Fuji"
Cohesion: 0.32
Nodes (8): Natural framing composition with blossoms, Pink cherry blossom branches framing scene, Lake Kawaguchi reflective water, Visitors walking on lakeside path, Mount Fuji framed by cherry blossoms over lake, Japan iconic landscape, Serene springtime hanami mood, Mount Fuji snow-capped peak

### Community 19 - "Sample: Lavender Field"
Cohesion: 0.33
Nodes (7): Stone farmhouse with cypress trees, Lavender field at sunset with Provencal farmhouse, Rows of blooming lavender, Leading lines composition from lavender rows, Serene pastoral mood, Provence countryside landscape, Pastel sunset sky over distant hills

### Community 20 - "Sample: Grand Canyon"
Cohesion: 0.33
Nodes (7): Wide elevated landscape composition with deep depth, Grand Canyon national park landscape, Rocky cliff edge in foreground with juniper shrubs, Bright blue sky with scattered white clouds, Grand Canyon vista with layered red rock formations under blue sky, Majestic, expansive, awe-inspiring natural mood, Vast canyon with stratified red and orange rock layers

### Community 21 - "Canyon Concept"
Cohesion: 0.67
Nodes (3): Grand Canyon vista with layered red rock formations under blue sky, Canyon landscape, Geological strata

### Community 22 - "Fuji Concept"
Cohesion: 0.67
Nodes (3): Mount Fuji framed by cherry blossoms over lake, Mount Fuji, Cherry blossoms (sakura)

## Knowledge Gaps
- **80 isolated node(s):** `AuthStatus`, `PhotoPlugin`, `DavEntry`, `CacheEntry`, `SlideshowCmd` (+75 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **3 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `is_image()` connect `Directory Plugin` to `Google Photos Plugin`?**
  _High betweenness centrality (0.018) - this node is a cross-community bridge._
- **What connects `AuthStatus`, `PhotoPlugin`, `DavEntry` to the rest of the system?**
  _80 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Directory Plugin` be split into smaller, more focused modules?**
  _Cohesion score 0.06 - nodes in this community are weakly interconnected._
- **Should `Image Cache & Amazon Plugin` be split into smaller, more focused modules?**
  _Cohesion score 0.05 - nodes in this community are weakly interconnected._
- **Should `WebDAV Plugin & Path Decoding` be split into smaller, more focused modules?**
  _Cohesion score 0.08 - nodes in this community are weakly interconnected._
- **Should `Amazon Photos Auth` be split into smaller, more focused modules?**
  _Cohesion score 0.13 - nodes in this community are weakly interconnected._
- **Should `Configuration System` be split into smaller, more focused modules?**
  _Cohesion score 0.12 - nodes in this community are weakly interconnected._