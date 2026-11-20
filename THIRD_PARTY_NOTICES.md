# Third-Party Notices

Device HUB bundles or derives from the following third-party works.

## Vendored code

| Package | Copyright | License | Notes |
| --- | --- | --- | --- |
| `vendor/gpui-pre`, `vendor/gpui-pre-windows` | Zed Industries | Apache-2.0 (`LICENSE-APACHE` in each source dir; `licenses/gpui-pre*-LICENSE-APACHE` in Windows packages) | GPUI fork with a GPU backdrop-filter patch; see `vendor/README.md` in the source tree |
| Shader material adapted from gpui-liquid-glass | 1234igor | MIT (`licenses/gpui-liquid-glass-LICENSE-MIT`) | DirectX adaptation of revision `c002c4d3758f934fdfaf173e8214a24eb65a05cd` |

## Bundled fonts (`crates/device-hub/fonts/`)

All embedded into `device-hub.exe` at build time.

| Font | Copyright | License |
| --- | --- | --- |
| Geist | The Geist Project Authors | SIL OFL 1.1 (`licenses/Geist-OFL.txt`) |
| IBM Plex Sans JP | IBM Corp. | SIL OFL 1.1 (`licenses/IBMPlexSansJP-OFL.txt`) |
| Zen Maru Gothic | The Zen Maru Gothic Authors | SIL OFL 1.1 (`licenses/ZenMaruGothic-OFL.txt`) |

## Icons

The UI includes SVG paths derived from [Lucide](https://lucide.dev) (ISC,
with MIT terms for icons derived from Feather). See `licenses/lucide-LICENSE`.

## Analysis material

The repository documents observed protocol behavior and includes device
report captures used as test fixtures. Copies of vendor web bundles and
extracted source files are kept outside the tracked source tree.

Device protocol support added in this fork was derived from **observed**
behavior only:

- **MCHOSE L7 Ultra** (`crates/devhub-hid/src/mchose.rs`): byte layouts and
  command codes were reverse-engineered from the public MCHOSE HUB web bundle
  (`www.mchose.com.cn`) and confirmed with USBPcap captures of the vendor app.
  No vendor source code was copied; the driver is an independent Rust
  re-implementation of the observed on-wire protocol.
- **HM66 (Sikakeyb Castle)**: serial frame format documented from USBPcap
  captures of the vendor Qt application (`hm-mag`). No vendor source code was
  copied or decompiled; only USB traffic was recorded.

The protocol facts themselves (command bytes, field offsets) are not
copyrightable expression, and all implementation code in this repository is
original, licensed under the project's dual MIT/Apache-2.0 terms.
