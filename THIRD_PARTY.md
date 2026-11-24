# Third-party licences

Wallora itself is MIT-licensed ([LICENSE](LICENSE)). This file lists what it builds on. Regenerate the crate table with:

```powershell
cargo deny --locked list --layout crate --format json
```

Generated 2026-11-23 from `Cargo.lock` at `main` (`3b782aa`). It covers the whole workspace graph, including the unshipped `spikes/iced-gallery` crate and build-time crates, so it over-approximates what the installer ships. Where a crate offers several licences (`A OR B`), you may use it under any one of them. `cargo deny check` enforces the allow-list in `deny.toml` in CI.

## Font

| Component | Licence | Obligations when bundling |
| --- | --- | --- |
| Salted Flutters Polaris (ppoolar) | Salted Flutters Polaris Font License v1.0 (custom, permissive; **not MIT**) | Ship an unmodified copy as `LICENSE.txt` alongside the font files. Keep the metadata line `Salted Flutters Polaris - Created by ppoolar` in every style. Do not sell the font on its own. See `docs/decisions.md` §6. Not yet bundled in the app. |

## Build tooling (not shipped inside the application)

- WiX Toolset 3.14.1 (MS-RL), used by `cargo-wix` 0.3.9 to build the MSI.
- `cargo-deny` 0.21.0 (MIT OR Apache-2.0), used for dependency policy checks.

## Rust crates

368 registry crates. Licence summary:

| Licence expression | Crates |
| --- | ---: |
| MIT OR Apache-2.0 | 193 |
| MIT | 73 |
| Apache-2.0 OR MIT | 23 |
| Unicode-3.0 | 18 |
| Apache-2.0 | 8 |
| BSD-3-Clause | 7 |
| MIT OR Apache-2.0 OR Zlib | 7 |
| BSD-2-Clause | 6 |
| Unlicense OR MIT | 4 |
| Zlib OR Apache-2.0 OR MIT | 3 |
| Zlib | 3 |
| ISC | 3 |
| BSL-1.0 | 2 |
| Apache-2.0 OR ISC OR MIT | 2 |
| MIT OR Zlib OR Apache-2.0 | 2 |
| BSD-3-Clause OR Apache-2.0 | 2 |
| BSD-2-Clause OR Apache-2.0 OR MIT | 2 |
| 0BSD OR MIT OR Apache-2.0 | 1 |
| Apache-2.0 OR MIT OR BSD-3-Clause | 1 |
| CC0-1.0 | 1 |
| CC0-1.0 OR Apache-2.0 | 1 |
| MPL-2.0 | 1 |
| Apache-2.0 OR ISC | 1 |
| Apache-2.0 OR BSL-1.0 | 1 |
| Apache-2.0 OR GPL-2.0-only | 1 |
| MIT OR Apache-2.0 OR Unicode-3.0 | 1 |
| CDLA-Permissive-2.0 | 1 |

| Crate | Version | Licence |
| --- | --- | --- |
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| aho-corasick | 1.1.5 | Unlicense OR MIT |
| aligned | 0.4.3 | MIT OR Apache-2.0 |
| aligned-vec | 0.6.4 | MIT |
| anyhow | 1.0.104 | MIT OR Apache-2.0 |
| arg_enum_proc_macro | 0.3.4 | MIT |
| arrayref | 0.3.9 | BSD-2-Clause |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 |
| as-slice | 0.2.1 | MIT OR Apache-2.0 |
| ash | 0.38.0+1.3.281 | MIT OR Apache-2.0 |
| async-compression | 0.4.52 | MIT OR Apache-2.0 |
| atomic-waker | 1.1.2 | Apache-2.0 OR MIT |
| autocfg | 1.5.1 | Apache-2.0 OR MIT |
| av-scenechange | 0.14.1 | MIT |
| av1-grain | 0.2.5 | BSD-2-Clause |
| avif-serialize | 0.8.9 | BSD-3-Clause |
| base64 | 0.22.1 | MIT OR Apache-2.0 |
| base64 | 0.23.1 | MIT OR Apache-2.0 |
| bit-set | 0.8.0 | Apache-2.0 OR MIT |
| bit-vec | 0.8.0 | Apache-2.0 OR MIT |
| bit_field | 0.10.4 | MIT OR Apache-2.0 |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| bitstream-io | 4.10.0 | MIT OR Apache-2.0 |
| built | 0.8.1 | MIT |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT |
| bytemuck_derive | 1.12.1 | Zlib OR Apache-2.0 OR MIT |
| byteorder | 1.5.0 | Unlicense OR MIT |
| byteorder-lite | 0.1.0 | Unlicense OR MIT |
| bytes | 1.12.2 | MIT |
| cc | 1.11.2 | MIT OR Apache-2.0 |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |
| cfg_aliases | 0.2.2 | MIT |
| clipboard-win | 5.4.3 | BSL-1.0 |
| codespan-reporting | 0.12.0 | Apache-2.0 |
| color_quant | 1.1.0 | MIT |
| compression-codecs | 0.4.47 | MIT OR Apache-2.0 |
| compression-core | 0.4.34 | MIT OR Apache-2.0 |
| core_detect | 1.0.0 | MIT OR Apache-2.0 |
| core_maths | 0.1.1 | MIT |
| cosmic-text | 0.15.0 | MIT OR Apache-2.0 |
| crc32fast | 1.5.5 | MIT OR Apache-2.0 |
| crossbeam-channel | 0.5.17 | MIT OR Apache-2.0 |
| crossbeam-deque | 0.8.8 | MIT OR Apache-2.0 |
| crossbeam-epoch | 0.9.21 | MIT OR Apache-2.0 |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 |
| cryoglyph | 0.1.0 | MIT OR Apache-2.0 OR Zlib |
| cursor-icon | 1.2.0 | MIT OR Apache-2.0 OR Zlib |
| deranged | 0.5.9 | MIT OR Apache-2.0 |
| directories | 6.0.0 | MIT OR Apache-2.0 |
| dirs-sys | 0.5.0 | MIT OR Apache-2.0 |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 |
| document-features | 0.2.12 | MIT OR Apache-2.0 |
| dpi | 0.1.2 | Apache-2.0 OR MIT |
| either | 1.19.0 | MIT OR Apache-2.0 |
| encoding_rs | 0.8.44 | Apache-2.0 OR MIT OR BSD-3-Clause |
| equator | 0.4.2 | MIT |
| equator-macro | 0.4.2 | MIT |
| equivalent | 1.0.3 | Apache-2.0 OR MIT |
| error-code | 3.4.0 | BSL-1.0 |
| etagere | 0.2.15 | MIT OR Apache-2.0 |
| euclid | 0.22.14 | MIT OR Apache-2.0 |
| exr | 1.74.3 | BSD-3-Clause |
| fallible-iterator | 0.3.0 | MIT OR Apache-2.0 |
| fallible-streaming-iterator | 0.1.9 | MIT OR Apache-2.0 |
| fax | 0.2.7 | MIT |
| fdeflate | 0.3.7 | MIT OR Apache-2.0 |
| find-msvc-tools | 0.1.15 | MIT OR Apache-2.0 |
| flate2 | 1.1.11 | MIT OR Apache-2.0 |
| fnv | 1.0.7 | Apache-2.0 OR MIT |
| foldhash | 0.1.5 | Zlib |
| foldhash | 0.2.0 | Zlib |
| font-types | 0.10.1 | MIT OR Apache-2.0 |
| font-types | 0.12.7 | MIT OR Apache-2.0 |
| fontdb | 0.23.0 | MIT |
| form_urlencoded | 1.2.2 | MIT OR Apache-2.0 |
| futures | 0.3.34 | MIT OR Apache-2.0 |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 |
| futures-core | 0.3.34 | MIT OR Apache-2.0 |
| futures-executor | 0.3.34 | MIT OR Apache-2.0 |
| futures-io | 0.3.34 | MIT OR Apache-2.0 |
| futures-macro | 0.3.34 | MIT OR Apache-2.0 |
| futures-sink | 0.3.34 | MIT OR Apache-2.0 |
| futures-task | 0.3.34 | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 |
| getrandom | 0.2.17 | MIT OR Apache-2.0 |
| getrandom | 0.4.3 | MIT OR Apache-2.0 |
| gif | 0.14.2 | MIT OR Apache-2.0 |
| gl_generator | 0.14.0 | Apache-2.0 |
| glam | 0.25.0 | MIT OR Apache-2.0 |
| glow | 0.16.0 | MIT OR Apache-2.0 OR Zlib |
| glutin_wgl_sys | 0.6.1 | Apache-2.0 |
| gpu-alloc | 0.6.2 | MIT OR Apache-2.0 |
| gpu-alloc-types | 0.3.1 | MIT OR Apache-2.0 |
| gpu-allocator | 0.27.0 | MIT OR Apache-2.0 |
| gpu-descriptor | 0.3.2 | MIT OR Apache-2.0 |
| gpu-descriptor-types | 0.2.0 | MIT OR Apache-2.0 |
| guillotiere | 0.6.2 | MIT OR Apache-2.0 |
| h2 | 0.4.20 | MIT |
| half | 2.7.1 | MIT OR Apache-2.0 |
| harfrust | 0.3.2 | MIT |
| hashbrown | 0.15.5 | MIT OR Apache-2.0 |
| hashbrown | 0.16.1 | MIT OR Apache-2.0 |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 |
| hashlink | 0.12.2 | MIT OR Apache-2.0 |
| hexf-parse | 0.2.1 | CC0-1.0 |
| http | 1.5.0 | MIT OR Apache-2.0 |
| http-body | 1.1.0 | MIT |
| http-body-util | 0.1.5 | MIT |
| httparse | 1.10.1 | MIT OR Apache-2.0 |
| httpdate | 1.0.3 | MIT OR Apache-2.0 |
| hyper | 1.12.0 | MIT |
| hyper-rustls | 0.27.10 | Apache-2.0 OR ISC OR MIT |
| hyper-util | 0.1.21 | MIT |
| iced | 0.14.0 | MIT |
| iced_core | 0.14.0 | MIT |
| iced_debug | 0.14.0 | MIT |
| iced_futures | 0.14.0 | MIT |
| iced_graphics | 0.14.0 | MIT |
| iced_program | 0.14.0 | MIT |
| iced_renderer | 0.14.0 | MIT |
| iced_runtime | 0.14.0 | MIT |
| iced_tiny_skia | 0.14.1 | MIT |
| iced_wgpu | 0.14.0 | MIT |
| iced_widget | 0.14.2 | MIT |
| iced_winit | 0.14.1 | MIT |
| icu_collections | 2.4.0 | Unicode-3.0 |
| icu_locale_core | 2.4.0 | Unicode-3.0 |
| icu_normalizer | 2.4.0 | Unicode-3.0 |
| icu_normalizer_data | 2.4.0 | Unicode-3.0 |
| icu_properties | 2.4.0 | Unicode-3.0 |
| icu_properties_data | 2.4.0 | Unicode-3.0 |
| icu_provider | 2.4.0 | Unicode-3.0 |
| idna | 1.1.0 | MIT OR Apache-2.0 |
| idna_adapter | 1.2.2 | Apache-2.0 OR MIT |
| image | 0.25.10 | MIT OR Apache-2.0 |
| image-webp | 0.2.4 | MIT OR Apache-2.0 |
| imgref | 1.12.3 | CC0-1.0 OR Apache-2.0 |
| indexmap | 2.14.2 | Apache-2.0 OR MIT |
| ipnet | 2.12.2 | MIT OR Apache-2.0 |
| itertools | 0.14.0 | MIT OR Apache-2.0 |
| itoa | 1.0.19 | MIT OR Apache-2.0 |
| kamadak-exif | 0.6.2 | BSD-2-Clause |
| keyring | 3.6.3 | MIT OR Apache-2.0 |
| khronos-egl | 6.0.0 | MIT OR Apache-2.0 |
| khronos_api | 3.1.0 | Apache-2.0 |
| kurbo | 0.10.4 | MIT OR Apache-2.0 |
| lazy_static | 1.5.1 | MIT OR Apache-2.0 |
| lebe | 0.5.3 | BSD-3-Clause |
| libc | 0.2.191 | MIT OR Apache-2.0 |
| libloading | 0.8.9 | ISC |
| libm | 0.2.16 | MIT |
| libsqlite3-sys | 0.40.0 | MIT |
| lilt | 0.8.2 | MIT |
| linebender_resource_handle | 0.1.1 | Apache-2.0 OR MIT |
| litemap | 0.8.3 | Unicode-3.0 |
| litrs | 1.0.1 | MIT OR Apache-2.0 |
| lock_api | 0.4.14 | MIT OR Apache-2.0 |
| log | 0.4.35 | MIT OR Apache-2.0 |
| loop9 | 0.1.5 | MIT |
| lru | 0.16.4 | MIT |
| matchers | 0.2.0 | MIT |
| maybe-rayon | 0.1.1 | MIT |
| memchr | 2.8.3 | Unlicense OR MIT |
| memmap2 | 0.9.12 | MIT OR Apache-2.0 |
| mime | 0.3.18 | MIT OR Apache-2.0 |
| miniz_oxide | 0.8.9 | MIT OR Zlib OR Apache-2.0 |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 |
| mio | 1.2.5 | MIT |
| moxcms | 0.8.1 | BSD-3-Clause OR Apache-2.0 |
| multiversion | 0.8.1 | MIT OR Apache-2.0 |
| multiversion-macros | 0.8.1 | MIT OR Apache-2.0 |
| multiversion_no_op | 1.0.0 | Apache-2.0 OR MIT |
| mutate_once | 0.1.2 | BSD-2-Clause |
| naga | 27.0.3 | MIT OR Apache-2.0 |
| new_debug_unreachable | 1.0.6 | MIT |
| no_std_io2 | 0.9.4 | Apache-2.0 OR MIT |
| nom | 8.0.0 | MIT |
| noop_proc_macro | 0.3.0 | MIT |
| nu-ansi-term | 0.50.3 | MIT |
| num-bigint | 0.4.8 | MIT OR Apache-2.0 |
| num-conv | 0.2.2 | MIT OR Apache-2.0 |
| num-derive | 0.4.2 | MIT OR Apache-2.0 |
| num-integer | 0.1.47 | MIT OR Apache-2.0 |
| num-rational | 0.4.2 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| option-ext | 0.2.0 | MPL-2.0 |
| ordered-float | 5.5.1 | MIT |
| parking_lot | 0.12.5 | MIT OR Apache-2.0 |
| parking_lot_core | 0.9.12 | MIT OR Apache-2.0 |
| paste | 1.0.15 | MIT OR Apache-2.0 |
| pastey | 0.1.1 | MIT OR Apache-2.0 |
| percent-encoding | 2.3.2 | MIT OR Apache-2.0 |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT |
| pkg-config | 0.3.34 | MIT OR Apache-2.0 |
| png | 0.18.1 | MIT OR Apache-2.0 |
| potential_utf | 0.1.6 | Unicode-3.0 |
| powerfmt | 0.2.1 | MIT OR Apache-2.0 |
| presser | 0.3.1 | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| profiling | 1.0.18 | MIT OR Apache-2.0 |
| profiling-procmacros | 1.0.18 | MIT OR Apache-2.0 |
| pxfm | 0.1.30 | BSD-3-Clause OR Apache-2.0 |
| qoi | 0.4.1 | MIT OR Apache-2.0 |
| quick-error | 2.0.1 | MIT OR Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| rand_core | 0.10.1 | MIT OR Apache-2.0 |
| range-alloc | 0.1.5 | MIT OR Apache-2.0 |
| rangemap | 1.8.0 | MIT OR Apache-2.0 |
| rav1e | 0.8.1 | BSD-2-Clause |
| ravif | 0.13.0 | BSD-3-Clause |
| raw-window-handle | 0.6.2 | MIT OR Apache-2.0 OR Zlib |
| rayon | 1.12.0 | MIT OR Apache-2.0 |
| rayon-core | 1.13.0 | MIT OR Apache-2.0 |
| read-fonts | 0.35.0 | MIT OR Apache-2.0 |
| read-fonts | 0.41.0 | MIT OR Apache-2.0 |
| regex-automata | 0.4.19 | MIT OR Apache-2.0 |
| regex-syntax | 0.8.12 | MIT OR Apache-2.0 |
| renderdoc-sys | 1.1.0 | MIT OR Apache-2.0 |
| reqwest | 0.12.28 | MIT OR Apache-2.0 |
| rgb | 0.8.53 | MIT |
| ring | 0.17.14 | Apache-2.0 OR ISC |
| rusqlite | 0.43.0 | MIT |
| rustc-hash | 1.1.0 | Apache-2.0 OR MIT |
| rustc-hash | 2.1.3 | Apache-2.0 OR MIT |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT |
| rustls-pki-types | 1.16.0 | MIT OR Apache-2.0 |
| rustls-webpki | 0.103.17 | ISC |
| rustversion | 1.0.23 | MIT OR Apache-2.0 |
| ryu | 1.0.23 | Apache-2.0 OR BSL-1.0 |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 |
| self_cell | 1.3.0 | Apache-2.0 OR GPL-2.0-only |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.154 | MIT OR Apache-2.0 |
| serde_spanned | 1.1.3 | MIT OR Apache-2.0 |
| serde_urlencoded | 0.7.1 | MIT OR Apache-2.0 |
| sharded-slab | 0.1.7 | MIT |
| shlex | 2.0.1 | MIT OR Apache-2.0 |
| simd-adler32 | 0.3.10 | MIT |
| simd_helpers | 0.1.0 | MIT |
| simdutf8 | 0.1.5 | MIT OR Apache-2.0 |
| skrifa | 0.37.0 | MIT OR Apache-2.0 |
| skrifa | 0.44.0 | MIT OR Apache-2.0 |
| slab | 0.4.12 | MIT |
| slotmap | 1.1.1 | Zlib |
| smallvec | 1.16.6 | MIT OR Apache-2.0 |
| smol_str | 0.2.2 | MIT OR Apache-2.0 |
| socket2 | 0.6.5 | MIT OR Apache-2.0 |
| softbuffer | 0.4.8 | MIT OR Apache-2.0 |
| spirv | 0.3.0+sdk-1.3.268.0 | Apache-2.0 |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 |
| strict-num | 0.1.1 | MIT |
| subtle | 2.6.1 | BSD-3-Clause |
| svg_fmt | 0.4.5 | MIT OR Apache-2.0 |
| swash | 0.2.10 | Apache-2.0 OR MIT |
| symlink | 0.1.0 | MIT OR Apache-2.0 |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.10 | MIT OR Apache-2.0 |
| sync_wrapper | 1.0.2 | Apache-2.0 |
| synstructure | 0.14.0 | MIT |
| sys-locale | 0.3.2 | MIT OR Apache-2.0 |
| target-features | 0.1.6 | MIT OR Apache-2.0 |
| thiserror | 1.0.69 | MIT OR Apache-2.0 |
| thiserror | 2.0.21 | MIT OR Apache-2.0 |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 |
| thread_local | 1.1.10 | MIT OR Apache-2.0 |
| tiff | 0.11.3 | MIT |
| time | 0.3.55 | MIT OR Apache-2.0 |
| time-core | 0.1.9 | MIT OR Apache-2.0 |
| tiny-skia | 0.11.4 | BSD-3-Clause |
| tiny-skia-path | 0.11.4 | BSD-3-Clause |
| tinystr | 0.8.5 | Unicode-3.0 |
| tinyvec | 1.13.3 | Zlib OR Apache-2.0 OR MIT |
| tokio | 1.54.1 | MIT |
| tokio-macros | 2.7.2 | MIT |
| tokio-rustls | 0.26.6 | MIT OR Apache-2.0 |
| tokio-util | 0.7.20 | MIT |
| toml | 1.2.1+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_datetime | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_parser | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 |
| tower | 0.5.3 | MIT |
| tower-http | 0.6.11 | MIT |
| tower-layer | 0.3.3 | MIT |
| tower-service | 0.3.3 | MIT |
| tracing | 0.1.44 | MIT |
| tracing-appender | 0.2.5 | MIT |
| tracing-attributes | 0.1.31 | MIT |
| tracing-core | 0.1.36 | MIT |
| tracing-log | 0.2.0 | MIT |
| tracing-subscriber | 0.3.23 | MIT |
| try-lock | 0.2.5 | MIT |
| ttf-parser | 0.25.1 | MIT OR Apache-2.0 |
| unicode-bidi | 0.3.18 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.27 | MIT OR Apache-2.0 OR Unicode-3.0 |
| unicode-linebreak | 0.1.5 | Apache-2.0 |
| unicode-script | 0.5.9 | MIT OR Apache-2.0 |
| unicode-segmentation | 1.13.3 | MIT OR Apache-2.0 |
| unicode-width | 0.2.3 | MIT OR Apache-2.0 |
| untrusted | 0.9.0 | ISC |
| url | 2.5.8 | MIT OR Apache-2.0 |
| utf8_iter | 1.1.0 | Apache-2.0 OR MIT |
| utf_types | 1.0.0 | Apache-2.0 OR MIT |
| uuid | 1.29.3 | Apache-2.0 OR MIT |
| v_frame | 0.3.9 | BSD-2-Clause |
| vcpkg | 0.2.15 | MIT OR Apache-2.0 |
| version_check | 0.9.5 | MIT OR Apache-2.0 |
| want | 0.3.2 | MIT |
| web-time | 1.1.0 | MIT OR Apache-2.0 |
| webpki-roots | 1.0.12 | CDLA-Permissive-2.0 |
| weezl | 0.1.12 | MIT OR Apache-2.0 |
| wgpu | 27.0.1 | MIT OR Apache-2.0 |
| wgpu-core | 27.0.3 | MIT OR Apache-2.0 |
| wgpu-core-deps-windows-linux-android | 27.0.0 | MIT OR Apache-2.0 |
| wgpu-hal | 27.0.4 | MIT OR Apache-2.0 |
| wgpu-types | 27.0.1 | MIT OR Apache-2.0 |
| window_clipboard | 0.5.1 | MIT |
| windows | 0.58.0 | MIT OR Apache-2.0 |
| windows | 0.62.2 | MIT OR Apache-2.0 |
| windows-collections | 0.3.2 | MIT OR Apache-2.0 |
| windows-core | 0.58.0 | MIT OR Apache-2.0 |
| windows-core | 0.62.2 | MIT OR Apache-2.0 |
| windows-future | 0.3.2 | MIT OR Apache-2.0 |
| windows-implement | 0.58.0 | MIT OR Apache-2.0 |
| windows-implement | 0.60.2 | MIT OR Apache-2.0 |
| windows-interface | 0.58.0 | MIT OR Apache-2.0 |
| windows-interface | 0.59.3 | MIT OR Apache-2.0 |
| windows-link | 0.2.1 | MIT OR Apache-2.0 |
| windows-numerics | 0.3.1 | MIT OR Apache-2.0 |
| windows-registry | 0.6.1 | MIT OR Apache-2.0 |
| windows-result | 0.2.0 | MIT OR Apache-2.0 |
| windows-result | 0.4.1 | MIT OR Apache-2.0 |
| windows-strings | 0.1.0 | MIT OR Apache-2.0 |
| windows-strings | 0.5.1 | MIT OR Apache-2.0 |
| windows-sys | 0.52.0 | MIT OR Apache-2.0 |
| windows-sys | 0.60.2 | MIT OR Apache-2.0 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 |
| windows-targets | 0.53.5 | MIT OR Apache-2.0 |
| windows-threading | 0.2.1 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.53.1 | MIT OR Apache-2.0 |
| winit | 0.30.13 | Apache-2.0 |
| winnow | 1.0.5 | MIT |
| winresource | 0.1.34 | MIT |
| writeable | 0.6.4 | Unicode-3.0 |
| xml-rs | 0.8.29 | MIT |
| y4m | 0.8.0 | MIT |
| yazi | 0.2.2 | Apache-2.0 OR MIT |
| yoke | 0.8.4 | Unicode-3.0 |
| yoke-derive | 0.8.4 | Unicode-3.0 |
| zeno | 0.3.3 | Apache-2.0 OR MIT |
| zerocopy | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| zerocopy-derive | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| zerofrom | 0.1.8 | Unicode-3.0 |
| zerofrom-derive | 0.1.8 | Unicode-3.0 |
| zeroize | 1.9.2 | Apache-2.0 OR MIT |
| zeroize_derive | 1.5.0 | Apache-2.0 OR MIT |
| zerotrie | 0.2.5 | Unicode-3.0 |
| zerovec | 0.11.9 | Unicode-3.0 |
| zerovec-derive | 0.11.7 | Unicode-3.0 |
| zmij | 1.0.23 | MIT |
| zune-core | 0.5.3 | MIT OR Apache-2.0 OR Zlib |
| zune-inflate | 0.2.54 | MIT OR Apache-2.0 OR Zlib |
| zune-jpeg | 0.5.15 | MIT OR Apache-2.0 OR Zlib |

Workspace crates (not listed above): iced-gallery, wallora-app, wallora-core, wallora-windows.
