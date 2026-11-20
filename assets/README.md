# Assets

| File | Use |
| --- | --- |
| `icon/master.png` | The app icon (1024 px), the source of everything else in `icon/` |
| `icon/png/icon-*.png` | Pre-scaled sizes embedded in the binary for the tray icon (`src/app_icon.rs`) |
| `icon/snapshot.ico` | The executable's icon, compiled in by `build.rs` from `snapshot.rc` |
| `character/kanban-musume.png` | Character art (cat-eared mascot), kept for future designs; not used by the app yet |

The icon and the character were generated with an image model for this project. To change the icon, replace `icon/master.png` and regenerate the sizes and the `.ico` from it (Lanczos downscaling to 16, 20, 24, 32, 40, 48, 64 and 256 px).
