# hofund brand assets

The hofund mark is the **Kernel Hilt**: an upright sword whose blade plugs into an
amber kernel block, with the crossguard formed by detachable plugin modules. It
stands for the project's microkernel and plugin architecture, and for Hofund, the
sword of Heimdall, guardian of the bridge between worlds.

## Files

| File | Use |
| --- | --- |
| `mark-light.svg` | Mark for light backgrounds |
| `mark-dark.svg` | Mark for dark backgrounds |
| `mark-mono.svg` | Single colour, uses `currentColor` |
| `icon.svg` | App icon tile (Ink tile, Paper mark, Amber kernel) |
| `tray-template.svg` | Black on transparent, for macOS template images |
| `lockup-light.svg` / `lockup-dark.svg` | Mark plus wordmark for light / dark backgrounds |
| `lockup-mono.svg` | Single colour lockup, uses `currentColor` |
| `pixel/icon-{16,22,32}.svg` | Pixel-snapped app icon for small sizes |
| `pixel/tray-template-{16,22,32}.svg` | Pixel-snapped tray template for small sizes |
| `png/` | Raster exports (512 / 1024 px marks and icons, 256 px tall lockups) |
| `social-preview.png` | 1280x640 GitHub social preview |

Desktop icons generated from these sources live in `apps/desktop/src-tauri/icons/`
(Tauri v2 layout; tray icons in `icons/tray/`).

## README hero

```html
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-dark.svg">
    <img alt="hofund" src="assets/brand/lockup-light.svg" width="320">
  </picture>
</p>
```

## Palette

| Name | Hex | Role |
| --- | --- | --- |
| Ink | `#0E1116` | Primary on light, icon tile, dark backgrounds |
| Paper | `#F6F5F1` | Primary on dark, light backgrounds |
| Amber | `#FFB224` | Single accent; reserved for the kernel block |

Amber is used once per mark, on the kernel only. Do not recolour the blade or
modules, add gradients, outlines, or shadows.

## Clear space

Keep clear space on every side equal to the kernel block height
(12 units on the 64-unit grid, about 19% of the mark height). For the lockup,
measure from the mark height.

## Minimum size

- Mark: 16 px. Below 48 px use the pixel-snapped files in `pixel/`; they keep
  the plugin modules visibly separated from the kernel.
- Lockup: 24 px tall. Below that, use the mark alone.

## Do not

- Stretch, rotate, or re-space the mark or wordmark.
- Place the colour mark on busy imagery or mid-tone backgrounds; use the mono
  version instead.
- Retype the wordmark; always use the supplied outlines.

## Typeface

The wordmark is set in **Geist SemiBold** by Vercel, licensed under the SIL Open
Font License 1.1. It is shipped only as converted outlines inside the SVGs; no
font files are included in this repository, so no font license file is required
here. Geist is available at https://github.com/vercel/geist-font.
