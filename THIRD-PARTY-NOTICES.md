# Third-party notices

Velochrome is released under the GNU General Public License v3.0 (see [LICENSE](LICENSE)). It
builds on the free software below, each used under its own licence. (The code's internal name is
"windedit"; the product is Velochrome.)

## Code and ideas adapted from other programs

- **darktable** (<https://www.darktable.org>, GPL-3.0-or-later): the embedded-metadata lens
  correction in `core/src/lens.rs` follows darktable's `src/iop/lens.cc` and `src/common/exif.cc`
  (the Panasonic and Olympus distortion and chromatic-aberration tags and how they are applied),
  which in turn follows what exiftool and exiv2 document about those tags;
  the noise reduction (`core/src/detail.rs`, `core/shaders/detail.hlsli`) follows its profiled
  wavelet denoiser, an à trous decomposition with the B3-spline kernel. Both were rewritten for
  this program's GPU pipeline rather than copied.
- **RawTherapee** (<https://rawtherapee.com>, GPL-3.0-or-later): the same noise reduction also
  follows RawTherapee's wavelet noise reduction.
- **The DNG specification** (Adobe): the camera-profile (DCP) reader and renderer in
  `core/src/profile.rs` implement the published format. No Adobe profiles are included: the app
  reads the ones already installed on the user's PC by Adobe software.

## Rust libraries (the engine, `core/`)

Versions are those in `Cargo.lock`. The list is made with `cargo metadata`; where a crate offers
a choice of licences ("MIT OR Apache-2.0"), Velochrome uses it under whichever of them is
compatible with GPL-3.0 (MIT, BSD, Zlib, Unlicense, and Apache-2.0 all are).

**Not permissive: `rawler` (the raw decoder) is LGPL-2.1.** LGPL-2.1 section 3 lets anyone apply
the ordinary GNU GPL to a combined work, so using it in this GPL-3.0 program is permitted. Its
source is at <https://github.com/dnglab/dnglab> (the version used is in the table below).
Everything else in the list is under permissive licences.

| Crate | Version | Licence |
| --- | --- | --- |
| [addr2line](https://crates.io/crates/addr2line) | 0.25.1 | Apache-2.0 OR MIT |
| [adler2](https://crates.io/crates/adler2) | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| [adler32](https://crates.io/crates/adler32) | 1.2.0 | Zlib |
| [allocator-api2](https://crates.io/crates/allocator-api2) | 0.2.21 | MIT OR Apache-2.0 |
| [alloc-no-stdlib](https://crates.io/crates/alloc-no-stdlib) | 2.0.4 | BSD-3-Clause |
| [alloc-stdlib](https://crates.io/crates/alloc-stdlib) | 0.2.4 | BSD-3-Clause |
| [android_system_properties](https://crates.io/crates/android_system_properties) | 0.1.6 | MIT OR Apache-2.0 |
| [autocfg](https://crates.io/crates/autocfg) | 1.5.1 | Apache-2.0 OR MIT |
| [backtrace](https://crates.io/crates/backtrace) | 0.3.76 | MIT OR Apache-2.0 |
| [bitflags](https://crates.io/crates/bitflags) | 2.13.2 | MIT OR Apache-2.0 |
| [bitstream-io](https://crates.io/crates/bitstream-io) | 4.10.0 | MIT/Apache-2.0 |
| [brotli-decompressor](https://crates.io/crates/brotli-decompressor) | 5.0.3 | BSD-3-Clause/MIT |
| [bumpalo](https://crates.io/crates/bumpalo) | 3.20.3 | MIT OR Apache-2.0 |
| [bytemuck](https://crates.io/crates/bytemuck) | 1.25.2 | Zlib OR Apache-2.0 OR MIT |
| [byteorder](https://crates.io/crates/byteorder) | 1.5.0 | Unlicense OR MIT |
| [byteorder-lite](https://crates.io/crates/byteorder-lite) | 0.1.0 | Unlicense OR MIT |
| [cc](https://crates.io/crates/cc) | 1.8.0 | MIT OR Apache-2.0 |
| [cfg-if](https://crates.io/crates/cfg-if) | 1.0.5 | MIT OR Apache-2.0 |
| [chrono](https://crates.io/crates/chrono) | 0.4.45 | MIT OR Apache-2.0 |
| [core-foundation-sys](https://crates.io/crates/core-foundation-sys) | 0.8.7 | MIT OR Apache-2.0 |
| [crc32fast](https://crates.io/crates/crc32fast) | 1.5.2 | MIT OR Apache-2.0 |
| [crossbeam-deque](https://crates.io/crates/crossbeam-deque) | 0.8.8 | MIT OR Apache-2.0 |
| [crossbeam-epoch](https://crates.io/crates/crossbeam-epoch) | 0.9.21 | MIT OR Apache-2.0 |
| [crossbeam-utils](https://crates.io/crates/crossbeam-utils) | 0.8.23 | MIT OR Apache-2.0 |
| [dary_heap](https://crates.io/crates/dary_heap) | 0.3.9 | MIT OR Apache-2.0 |
| [either](https://crates.io/crates/either) | 1.19.0 | MIT OR Apache-2.0 |
| [enumn](https://crates.io/crates/enumn) | 0.1.14 | MIT OR Apache-2.0 |
| [equivalent](https://crates.io/crates/equivalent) | 1.0.3 | Apache-2.0 OR MIT |
| [fdeflate](https://crates.io/crates/fdeflate) | 0.3.7 | MIT OR Apache-2.0 |
| [find-msvc-tools](https://crates.io/crates/find-msvc-tools) | 0.1.14 | MIT OR Apache-2.0 |
| [flate2](https://crates.io/crates/flate2) | 1.1.10 | MIT OR Apache-2.0 |
| [foldhash](https://crates.io/crates/foldhash) | 0.2.0 | Zlib |
| [futures-core](https://crates.io/crates/futures-core) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-task](https://crates.io/crates/futures-task) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-util](https://crates.io/crates/futures-util) | 0.3.34 | MIT OR Apache-2.0 |
| [getrandom](https://crates.io/crates/getrandom) | 0.4.3 | MIT OR Apache-2.0 |
| [gimli](https://crates.io/crates/gimli) | 0.32.3 | MIT OR Apache-2.0 |
| [glob](https://crates.io/crates/glob) | 0.3.4 | MIT OR Apache-2.0 |
| [hashbrown](https://crates.io/crates/hashbrown) | 0.16.1 | MIT OR Apache-2.0 |
| [hashbrown](https://crates.io/crates/hashbrown) | 0.17.1 | MIT OR Apache-2.0 |
| [hex](https://crates.io/crates/hex) | 0.4.3 | MIT OR Apache-2.0 |
| [iana-time-zone](https://crates.io/crates/iana-time-zone) | 0.1.65 | MIT OR Apache-2.0 |
| [iana-time-zone-haiku](https://crates.io/crates/iana-time-zone-haiku) | 0.1.2 | MIT OR Apache-2.0 |
| [image](https://crates.io/crates/image) | 0.25.10 | MIT OR Apache-2.0 |
| [image-webp](https://crates.io/crates/image-webp) | 0.2.4 | MIT OR Apache-2.0 |
| [indexmap](https://crates.io/crates/indexmap) | 2.14.2 | Apache-2.0 OR MIT |
| [itertools](https://crates.io/crates/itertools) | 0.14.0 | MIT OR Apache-2.0 |
| [js-sys](https://crates.io/crates/js-sys) | 0.3.106 | MIT OR Apache-2.0 |
| [jxl-bitstream](https://crates.io/crates/jxl-bitstream) | 1.1.0 | MIT OR Apache-2.0 |
| [jxl-coding](https://crates.io/crates/jxl-coding) | 1.0.1 | MIT OR Apache-2.0 |
| [jxl-color](https://crates.io/crates/jxl-color) | 0.11.0 | MIT OR Apache-2.0 |
| [jxl-frame](https://crates.io/crates/jxl-frame) | 0.13.3 | MIT OR Apache-2.0 |
| [jxl-grid](https://crates.io/crates/jxl-grid) | 0.6.2 | MIT OR Apache-2.0 |
| [jxl-image](https://crates.io/crates/jxl-image) | 0.13.0 | MIT OR Apache-2.0 |
| [jxl-jbr](https://crates.io/crates/jxl-jbr) | 0.2.1 | MIT OR Apache-2.0 |
| [jxl-modular](https://crates.io/crates/jxl-modular) | 0.11.3 | MIT OR Apache-2.0 |
| [jxl-oxide](https://crates.io/crates/jxl-oxide) | 0.12.6 | MIT OR Apache-2.0 |
| [jxl-oxide-common](https://crates.io/crates/jxl-oxide-common) | 1.0.0 | MIT OR Apache-2.0 |
| [jxl-render](https://crates.io/crates/jxl-render) | 0.12.4 | MIT OR Apache-2.0 |
| [jxl-threadpool](https://crates.io/crates/jxl-threadpool) | 1.0.0 | MIT OR Apache-2.0 |
| [jxl-vardct](https://crates.io/crates/jxl-vardct) | 0.11.1 | MIT OR Apache-2.0 |
| [lazy_static](https://crates.io/crates/lazy_static) | 1.5.1 | MIT OR Apache-2.0 |
| [libc](https://crates.io/crates/libc) | 0.2.191 | MIT OR Apache-2.0 |
| [libflate](https://crates.io/crates/libflate) | 2.3.2 | MIT |
| [libflate_lz77](https://crates.io/crates/libflate_lz77) | 2.3.0 | MIT |
| [log](https://crates.io/crates/log) | 0.4.34 | MIT OR Apache-2.0 |
| [md5](https://crates.io/crates/md5) | 0.8.1 | Apache-2.0 OR MIT |
| [memchr](https://crates.io/crates/memchr) | 2.8.3 | Unlicense OR MIT |
| [memmap2](https://crates.io/crates/memmap2) | 0.9.11 | MIT OR Apache-2.0 |
| [miniz_oxide](https://crates.io/crates/miniz_oxide) | 0.8.9 | MIT OR Zlib OR Apache-2.0 |
| [miniz_oxide](https://crates.io/crates/miniz_oxide) | 0.9.1 | MIT OR Zlib OR Apache-2.0 |
| [moxcms](https://crates.io/crates/moxcms) | 0.8.1 | BSD-3-Clause OR Apache-2.0 |
| [multiversion](https://crates.io/crates/multiversion) | 0.8.0 | MIT OR Apache-2.0 |
| [multiversion-macros](https://crates.io/crates/multiversion-macros) | 0.8.0 | MIT OR Apache-2.0 |
| [no_std_io2](https://crates.io/crates/no_std_io2) | 0.9.4 | Apache-2.0 OR MIT |
| [num](https://crates.io/crates/num) | 0.4.3 | MIT OR Apache-2.0 |
| [num_enum](https://crates.io/crates/num_enum) | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 |
| [num_enum_derive](https://crates.io/crates/num_enum_derive) | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 |
| [num-bigint](https://crates.io/crates/num-bigint) | 0.4.8 | MIT OR Apache-2.0 |
| [num-complex](https://crates.io/crates/num-complex) | 0.4.6 | MIT OR Apache-2.0 |
| [num-integer](https://crates.io/crates/num-integer) | 0.1.47 | MIT OR Apache-2.0 |
| [num-iter](https://crates.io/crates/num-iter) | 0.1.46 | MIT OR Apache-2.0 |
| [num-rational](https://crates.io/crates/num-rational) | 0.4.2 | MIT OR Apache-2.0 |
| [num-traits](https://crates.io/crates/num-traits) | 0.2.19 | MIT OR Apache-2.0 |
| [object](https://crates.io/crates/object) | 0.37.3 | Apache-2.0 OR MIT |
| [once_cell](https://crates.io/crates/once_cell) | 1.21.4 | MIT OR Apache-2.0 |
| [pin-project-lite](https://crates.io/crates/pin-project-lite) | 0.2.17 | Apache-2.0 OR MIT |
| [png](https://crates.io/crates/png) | 0.18.1 | MIT OR Apache-2.0 |
| [proc-macro2](https://crates.io/crates/proc-macro2) | 1.0.107 | MIT OR Apache-2.0 |
| [proc-macro-crate](https://crates.io/crates/proc-macro-crate) | 3.5.0 | MIT OR Apache-2.0 |
| [pxfm](https://crates.io/crates/pxfm) | 0.1.30 | BSD-3-Clause OR Apache-2.0 |
| [quick-error](https://crates.io/crates/quick-error) | 2.0.1 | MIT/Apache-2.0 |
| [quote](https://crates.io/crates/quote) | 1.0.47 | MIT OR Apache-2.0 |
| [rawler](https://crates.io/crates/rawler) | 0.8.0 | LGPL-2.1 |
| [rayon](https://crates.io/crates/rayon) | 1.12.0 | MIT OR Apache-2.0 |
| [rayon-core](https://crates.io/crates/rayon-core) | 1.13.0 | MIT OR Apache-2.0 |
| [r-efi](https://crates.io/crates/r-efi) | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| [rle-decode-fast](https://crates.io/crates/rle-decode-fast) | 1.0.3 | MIT OR Apache-2.0 |
| [rustc_version](https://crates.io/crates/rustc_version) | 0.4.1 | MIT OR Apache-2.0 |
| [rustc-demangle](https://crates.io/crates/rustc-demangle) | 0.1.28 | MIT/Apache-2.0 |
| [rustversion](https://crates.io/crates/rustversion) | 1.0.23 | MIT OR Apache-2.0 |
| [semver](https://crates.io/crates/semver) | 1.0.28 | MIT OR Apache-2.0 |
| [serde](https://crates.io/crates/serde) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_core](https://crates.io/crates/serde_core) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_derive](https://crates.io/crates/serde_derive) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_spanned](https://crates.io/crates/serde_spanned) | 0.6.9 | MIT OR Apache-2.0 |
| [shlex](https://crates.io/crates/shlex) | 2.0.1 | MIT OR Apache-2.0 |
| [simd-adler32](https://crates.io/crates/simd-adler32) | 0.3.10 | MIT |
| [slab](https://crates.io/crates/slab) | 0.4.12 | MIT |
| [syn](https://crates.io/crates/syn) | 2.0.119 | MIT OR Apache-2.0 |
| [syn](https://crates.io/crates/syn) | 3.0.9 | MIT OR Apache-2.0 |
| [target-features](https://crates.io/crates/target-features) | 0.1.6 | MIT OR Apache-2.0 |
| [thiserror](https://crates.io/crates/thiserror) | 2.0.21 | MIT OR Apache-2.0 |
| [thiserror-impl](https://crates.io/crates/thiserror-impl) | 2.0.21 | MIT OR Apache-2.0 |
| [toml](https://crates.io/crates/toml) | 0.8.23 | MIT OR Apache-2.0 |
| [toml_datetime](https://crates.io/crates/toml_datetime) | 0.6.11 | MIT OR Apache-2.0 |
| [toml_datetime](https://crates.io/crates/toml_datetime) | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 |
| [toml_edit](https://crates.io/crates/toml_edit) | 0.22.27 | MIT OR Apache-2.0 |
| [toml_edit](https://crates.io/crates/toml_edit) | 0.25.17+spec-1.1.0 | MIT OR Apache-2.0 |
| [toml_parser](https://crates.io/crates/toml_parser) | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 |
| [toml_write](https://crates.io/crates/toml_write) | 0.1.2 | MIT OR Apache-2.0 |
| [tracing](https://crates.io/crates/tracing) | 0.1.44 | MIT |
| [tracing-core](https://crates.io/crates/tracing-core) | 0.1.36 | MIT |
| [unicode-ident](https://crates.io/crates/unicode-ident) | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| [uuid](https://crates.io/crates/uuid) | 1.28.0 | Apache-2.0 OR MIT |
| [wasm-bindgen](https://crates.io/crates/wasm-bindgen) | 0.2.129 | MIT OR Apache-2.0 |
| [wasm-bindgen-macro](https://crates.io/crates/wasm-bindgen-macro) | 0.2.129 | MIT OR Apache-2.0 |
| [wasm-bindgen-macro-support](https://crates.io/crates/wasm-bindgen-macro-support) | 0.2.129 | MIT OR Apache-2.0 |
| [wasm-bindgen-shared](https://crates.io/crates/wasm-bindgen-shared) | 0.2.129 | MIT OR Apache-2.0 |
| [weezl](https://crates.io/crates/weezl) | 0.1.12 | MIT OR Apache-2.0 |
| [windows](https://crates.io/crates/windows) | 0.62.2 | MIT OR Apache-2.0 |
| [windows-collections](https://crates.io/crates/windows-collections) | 0.3.2 | MIT OR Apache-2.0 |
| [windows-core](https://crates.io/crates/windows-core) | 0.62.2 | MIT OR Apache-2.0 |
| [windows-future](https://crates.io/crates/windows-future) | 0.3.2 | MIT OR Apache-2.0 |
| [windows-implement](https://crates.io/crates/windows-implement) | 0.60.2 | MIT OR Apache-2.0 |
| [windows-interface](https://crates.io/crates/windows-interface) | 0.59.3 | MIT OR Apache-2.0 |
| [windows-link](https://crates.io/crates/windows-link) | 0.2.1 | MIT OR Apache-2.0 |
| [windows-numerics](https://crates.io/crates/windows-numerics) | 0.3.1 | MIT OR Apache-2.0 |
| [windows-result](https://crates.io/crates/windows-result) | 0.4.1 | MIT OR Apache-2.0 |
| [windows-strings](https://crates.io/crates/windows-strings) | 0.5.1 | MIT OR Apache-2.0 |
| [windows-threading](https://crates.io/crates/windows-threading) | 0.2.1 | MIT OR Apache-2.0 |
| [winnow](https://crates.io/crates/winnow) | 0.7.15 | MIT |
| [winnow](https://crates.io/crates/winnow) | 1.0.4 | MIT |
| [zerocopy](https://crates.io/crates/zerocopy) | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| [zerocopy-derive](https://crates.io/crates/zerocopy-derive) | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| [zlib-rs](https://crates.io/crates/zlib-rs) | 0.6.8 | Zlib |
| [zune-core](https://crates.io/crates/zune-core) | 0.5.3 | MIT OR Apache-2.0 OR Zlib |
| [zune-jpeg](https://crates.io/crates/zune-jpeg) | 0.5.15 | MIT OR Apache-2.0 OR Zlib |

The full licence texts are in each crate's own repository and in the crates' source packages
(`~/.cargo/registry`); the permissive licences require that their notices be kept, which this
file does as part of the distribution.

## .NET and Windows libraries (the app, `app/Windedit/`)

- **Windows App SDK and WinUI 3** (`Microsoft.WindowsAppSDK.WinUI`, `.Foundation`, `.Runtime`;
  <https://github.com/microsoft/WindowsAppSDK>, <https://github.com/microsoft/microsoft-ui-xaml>):
  Copyright (c) Microsoft Corporation, MIT License. In the Microsoft Store package the runtime comes as
  Microsoft's own framework package.
- **.NET runtime and libraries** (<https://github.com/dotnet/runtime>): Copyright (c) .NET
  Foundation and Contributors, MIT License.
- `Microsoft.Windows.SDK.BuildTools` is used only to build the app; none of it is distributed.

```
MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy of this software
and associated documentation files (the "Software"), to deal in the Software without
restriction, including without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

## Build tools (not distributed)

The shaders are compiled at build time with `dxc` from the Windows SDK; the compiled shaders
are part of the engine DLL.