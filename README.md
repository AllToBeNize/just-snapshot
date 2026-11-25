# just-snapshot

轻量的 Windows 截图工具，Rust + 原生 Win32 实现，常驻托盘时几乎不占资源。支持 Windows 10 / 11 x64。

| 功能 | 一句话 |
| --- | --- |
| 智能选区 | 自动吸附窗口、控件和内容区，侧键 / 滚轮逐层切换，也能自由拖框、输入宽高 |
| 标注 | 矩形、文本、画笔、马赛克、箭头、序号、放大镜；画好的标注可以重新选中修改，支持撤回 / 重做 |
| 长截图 | 边滚动边拼接，实时预览 |
| 录像 | 区域录像或全屏录像存为 MP4，可录系统声音和麦克风，录完一键转 GIF |
| 按键显示 | 屏幕上悬浮键鼠 / Xbox 手柄面板，按下的键高亮 |
| 贴图 | 把截图钉在屏幕最前面，可缩放、调透明度、分组 |
| 提取文字 | 离线识别文字和二维码 / 条形码，在贴图上直接拖选复制；可选更准的 PaddleOCR 引擎 |
| 翻译 | 识别后翻译，译文盖在原文位置；默认免费服务，也可接 OpenAI 兼容接口、DeepL、百度、有道 |
| 识图 | 把截图交给 Google Lens 或 Yandex 以图搜图，结果在浏览器打开；也能接自己的识图接口 |
| 找回上次截图 | 误按 `Esc` 后一键回到上次的画面、选区和标注 |
| 复制即文件 | 复制的截图在资源管理器里 `Ctrl+V` 就是 PNG 文件 |

## 安装

到 [Releases](https://github.com/AllToBeNize/just-snapshot/releases) 下载：

- **安装版** `just-snapshot-v<版本>-setup.exe`：默认装到 `%LOCALAPPDATA%\Programs`，不需要管理员权限；选别的位置时只要选父文件夹（比如 `D:\`），会自动在里面建 `just-snapshot` 文件夹。在 Windows「设置 → 应用」里卸载，截图和录像不会被删。
- **免安装版** `just-snapshot-v<版本>-windows-x64.zip`：解压后运行 `just-snapshot.exe`。

程序驻留在系统托盘，首次运行会加入开机启动（可在设置里关掉）。新版本通过程序内的自动更新获取，不用重新安装。

## 快捷键

所有快捷键都能在设置里改。

| 全局 | 默认 |
| --- | --- |
| 截图 | `Ctrl+Alt+A` |
| 截取当前窗口 | `Ctrl+Alt+W` |
| 找回上次截图 | `Ctrl+Alt+R` |
| 区域录像 开始 / 停止 | `Ctrl+Alt+Z` |
| 暂停录像（录制中才占用） | `Ctrl+Alt+P` |
| 全屏 / 游戏录像 | `Alt+F8` |
| 键鼠面板 / 手柄面板 | `Ctrl+Alt+K` / `Ctrl+Alt+G` |

| 截图界面 | 默认 |
| --- | --- |
| 矩形 · 文本 · 画笔 · 马赛克 · 箭头 · 序号 · 放大镜 · 选择 | `Ctrl+1` ～ `Ctrl+8` |
| 撤回 · 重做 | `Ctrl+Z` · `Ctrl+Y` / `Ctrl+Shift+Z` |
| 提取文本 · 翻译 · 识图 | `Ctrl+E` · `Ctrl+F` · `Ctrl+I` |
| 长截图 · 贴图 · 保存 | `Ctrl+L` · `Ctrl+T` · `Ctrl+S` |
| 复制 · 退出 | `Ctrl+C` / `Enter` / 双击 · `Esc` |
| 选中整个屏幕 | `Ctrl+A` |
| 截图带上鼠标指针（开关） | `T` |
| 显示 / 隐藏快捷键提示面板 | `G` |
| 复制鼠标下的颜色 `#RRGGBB` | `C` |

截图界面左下角的提示面板会列出当前能用的全部快捷键。按 `G` 收起成一行小条，再按 `G` 展开，下次截图沿用。

## 截图

1. 按 `Ctrl+Alt+A`，移动鼠标自动预选窗口、控件或内容区。鼠标侧键「后退 / 前进」（或滚轮、`Alt+←/→`）在外层和内层之间切换，单击锁定；也可以按住左键拖出自由选区。
2. 锁定后拖内部移动、拖边角缩放；方向键移动 1 像素（`Shift` 10 像素），`Ctrl+方向键` 改大小。单击左上方的尺寸标签可以直接输入 `800 600`。
3. 右键清除选区，再右键退出。

### 标注

选中工具后，工具栏下方出现参数栏（粗细、字号、颜色等）。再点一次当前工具或按右键退出标注。

- **矩形**：可开圆角、虚线。
- **文本**：可设对齐、描边、底色；拖四边改文本框宽高，拖四角连字号一起缩放。`Ctrl+Enter` 确认。
- **画笔 / 马赛克 / 箭头**：滚轮可快速调画笔粗细。
- **序号**：单击放一个自动递增的数字圆点。
- **放大镜**：按下后向外拖出范围，旁边生成放大的圆形镜头（2–6 倍）。
- **选择**：单击、`Ctrl+单击` 或框选多个标注，一起移动、删除。

不选工具时单击已有标注即可重新编辑：拖动移动，拖控制点调整，参数栏改样式，`Delete` 删除。

## 长截图

框住会滚动的内容，按 `Ctrl+L`，然后在框内滚动滚轮，旁边实时预览拼接结果，复制 / 保存和普通截图一样。预览停住时往回滚一点让前后两屏重叠即可接上。渐变背景、半透明卡片的聊天窗口也能拼，悬浮按钮只保留一份。

## 录像

- **区域录像** `Ctrl+Alt+Z`：像截图一样框选（画面不冻结），在录像工具栏上开关系统声音、麦克风、键鼠 / 手柄面板，选帧率和画质，按 `Enter` 开始。再按 `Ctrl+Alt+Z` 停止保存，之后可以打开、打开文件夹或转 GIF。
- **全屏 / 游戏录像** `Alt+F8`：直接录前台窗口所在的整块屏幕，不框选不倒数，再按一次停止。

两种录像的控制栏上都有键鼠、手柄两个开关，录制中也能随时显示或隐藏面板，下次录像沿用。

红框和控制栏不会被录进视频。默认 60 帧原画，优先用显卡硬件编码。录像默认存在程序目录下的 `Videos` 文件夹，可以在设置「录像与按键显示」页修改位置、帧率、画质、默认声音和倒数 3 秒。

## 按键显示

`Ctrl+Alt+K` / `Ctrl+Alt+G` 开关键鼠和手柄面板。面板不挡鼠标，会被录像和直播捕捉到。

- 面板最下面一条是拖动条（不会被录进去）：拖「≡」放到任意位置，在上面滚轮调大小，`Ctrl+滚轮` 调不透明度；
- 点拖动条右端的「›」展开小面板，直接点九宫格选位置、调大小和不透明度；区域录像工具栏下面也能调位置和不透明度。

面板关闭时不监听任何输入。以管理员身份运行的程序（部分游戏）里的按键收不到。

## 提取文字与翻译

- **提取文字** `Ctrl+E`：截图变成贴图并离线识别文字。在文字上拖动逐字选择（点贴图别处或切到别的窗口就取消），双击选一个词（像终端一样到空格为止，`UI_TabControl`、路径都算一个词），三击选整行，`Ctrl+C` 复制选中或全部。行首的图标（比如 UE 大纲里的小图标）不会被当成文字。二维码 / 条形码会用蓝框标出，单击复制内容，`Ctrl+单击` 打开网址。

### 识别引擎

在设置的「识别与服务」页选（「提取文本」一行的齿轮也会跳到这里）：

| 档位 | 引擎 | 特点 |
| --- | --- | --- |
| 快速（默认） | Windows 自带 OCR | 立即可用、速度快，语言跟随系统已装语言包；小字、反色文字和代码名容易认错 |
| 准确 | [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR) PP-OCRv5（ONNX Runtime 本地运行） | 中英混排、代码名、小字明显更准，每次多花几秒；第一次选它时下载约 37 MB 的识别包（运行库和模型，逐个校验 SHA-256），之后全程离线 |

识别包放在 `%LOCALAPPDATA%\just-snapshot\ocr\pp-ocrv5\`。国内源优先：模型从魔搭社区（RapidAI 官方仓库）下载，运行库从清华 / 阿里云 PyPI 镜像上微软官方的 onnxruntime 包里取；都不通时再走 GitHub 的 `ocr-pack-v1` 发布和 `update.txt` 里的镜像。不管从哪里下，每个文件都要和登记的大小、SHA-256 一致才会用。
- **翻译** `Ctrl+F`：识别后翻译，译文按原底色、原字色盖在原文上，左上角按钮切换原文 / 译文。旁边小窗显示完整译文，可切换目标语言（默认中译英、其他译中）、复制、出错重试。

### 翻译服务

默认免费服务，不用密钥。在设置「翻译」一行的齿轮里切换：

| 服务商 | 需要填 | 说明 |
| --- | --- | --- |
| 免费翻译 | 无（邮箱可选） | [MyMemory](https://mymemory.translated.net/doc/spec.php) 公共接口，每天约 5000 字，填邮箱约 5 万字，质量一般 |
| OpenAI 兼容 | 接口地址、模型、API Key | 支持 `/chat/completions` 的接口均可（OpenAI、DeepSeek、通义千问、Kimi、Ollama 等），本地模型可不填 Key |
| DeepL | API Key | 以 `:fx` 结尾的免费版密钥自动走免费接口 |
| 百度翻译 | APP ID、密钥 | 通用文本翻译 |
| 有道翻译 | 应用 ID、应用密钥 | 有道智云文本翻译 |

密钥只存在 Windows 凭据管理器（名称以 `just-snapshot/translate/` 开头），不写进任何文件。

## 识图

工具栏的「识图」`Ctrl+I` 在按钮下弹出引擎菜单：选 Google、Yandex 或自己加的引擎，或者「全部引擎」同时搜、每个引擎开一个标签页。上传时工具栏上显示「正在用 … 识图」，截图界面先留着；结果页在默认浏览器打开后截图界面才关闭，失败时弹出原因，可以直接再试。贴图的右键菜单和托盘菜单「识图上次截图」里有同样的引擎菜单。

引擎在设置「识图」一行的齿轮里改（菜单最后一项「设置识图接口…」也能打开）。每个引擎是一组字段：

| 字段 | 说明 |
| --- | --- |
| 上传地址 | 截图以 multipart 表单 POST 到这里；`{timestamp}` 换成当前毫秒数 |
| 文件字段 | 图片放在哪个表单字段 |
| 结果字段 | 留空：由浏览器上传，跟着服务器的跳转打开结果页（Google Lens 这样，它的结果只认上传时的浏览器会话）。填了：由程序上传，按路径从返回的 JSON 取值，如 `blocks.0.params.cbirId`（Yandex 这样） |
| 结果链接 | 结果页地址，`{value}` 换成取到的值（自动 URL 编码）；留空表示取到的值本身就是网址 |

「测试」用程序图标搜一次，在浏览器里打开结果。浏览器上传时程序会在临时文件夹写一个内嵌截图的小网页（`%TEMP%\just-snapshot-search\`），十分钟后的下一次识图时删掉。内置的 Google、Yandex 可以改，也能「恢复默认」；自己加的可以删除。

## 贴图

`Ctrl+T` 把截图钉在屏幕最前面：左键拖动，滚轮缩放，`Ctrl+滚轮` 调透明度，`Esc` 或双击关闭。右键菜单可以复制、保存、提取文字、翻译、识图、置顶、分组、隐藏；托盘菜单「贴图」里按组显示 / 隐藏或全部关闭（「全部显示」也会找回单独隐藏的贴图），「找回关闭的贴图」能把最近关掉的 10 张贴图按原来的位置、大小和透明度找回来（只在本次运行期间保留）。

- **选中与多选**：和资源管理器一样，单击选中一张，`Ctrl+单击` 加入或移出选中，选中的贴图边缘是一圈淡红色光晕，鼠标悬停时是红蓝渐变的淡光晕。拖动选中的任意一张，选中的会一起移动；滚轮缩放、`Ctrl+滚轮` 调透明度、`Esc` 关闭也都作用于全部选中的；右键菜单里的置顶、隐藏、分组（「把选中的 N 张放进新组」）、关闭同样一起生效。
- **置顶**：贴图本来就在所有窗口前面；「置顶」是让这张贴图始终压在其他贴图上面。点右上角的图钉切换（鼠标移上去才出现，钉下去之后一直显示），右键菜单里也有。

## 设置

右键托盘图标 →「设置」：

- **快捷键**：全局键和工具栏键都能改，截图键可加一个副键位。冲突的按键当场标出；截图键被其他程序占用时托盘图标显示橙色感叹号，双击托盘图标仍可截图。
- **保存**：保存位置、文件名模板（`{date}` `{time}` `{w}` `{h}`）、自动保存、保存时加阴影 / 边框（只作用于文件，不影响剪贴板）。
- **外观**：石墨（默认）、午夜蓝、墨绿、浅色四套配色。
- **其他**：开机启动、自动检查更新、贴图透明度步长。

### 自动更新

启动 30 秒后检查 GitHub 上的新版本，有更新时右下角弹卡片说明这一版改了什么，可以立即更新、稍后或跳过此版本。下载的文件经签名和 SHA-256 校验后才替换，失败时旧版本照常可用。托盘菜单「检查更新」可以手动查。

### 配置文件

都在 `%LOCALAPPDATA%\just-snapshot\` 下，纯文本：

| 文件 | 内容 |
| --- | --- |
| `shortcuts.txt` | 快捷键、主题、贴图透明度步长、是否自动更新 |
| `output.txt` | 保存位置、文件名模板、自动保存、阴影、边框 |
| `recording.txt` | 录像位置、帧率、画质、声音和鼠标指针默认值 |
| `overlay.txt` | 按键面板的显示状态、位置、大小、不透明度 |
| `translate.txt` | 翻译服务商和非密钥参数 |
| `image_search.txt` | 识图引擎（没有这个文件时用内置的 Google、Yandex） |
| `ocr.txt` | 文字识别档位（快速 / 准确） |
| `update.txt` | 更新镜像（`mirrors=https://镜像1/;https://镜像2/`，GitHub 失败后依次尝试）和跳过的版本 |

## 扩展开发

### 构建与测试

在 Windows 上安装 [Rust MSVC 工具链](https://rustup.rs/) 和 Visual Studio C++ 生成工具（含 Windows SDK）：

| 命令 | 作用 |
| --- | --- |
| `.\snapshot.ps1 build` | Release 构建，产物在 `target/x86_64-pc-windows-msvc/release/just-snapshot.exe` |
| `.\snapshot.ps1 test` | 运行全部单元测试 |
| `.\snapshot.ps1 package` | 测试 + 构建，打出安装包和 ZIP 到 `dist/` |

- 重新构建前先从托盘退出正在运行的 just-snapshot，否则产物被锁。
- 在 `target` 文件夹里运行的开发版不会自动更新。
- 远程 / 非交互 PowerShell 里请从 `cmd` 调用：`powershell -NoProfile -ExecutionPolicy Bypass -File snapshot.ps1 test > log 2>&1`。

### 代码结构

纯 Win32 + GDI / GDI+ 自绘，没有 UI 框架。空闲时只有托盘消息循环，截图用到的缓冲、GPU 资源和线程都按次创建、用完释放。

| 领域 | 主要模块（`src/`） |
| --- | --- |
| 托盘与快捷键 | `main.rs`、`global_hotkey.rs`、`hotkey.rs` |
| 截图会话与选区 | `selection/`、`targets.rs`、`selection_geometry.rs`、`capture.rs` |
| 工具栏与标注 | `toolbar.rs`、`annotation_options.rs`、`annotation.rs`、`text_mark.rs`、`mosaic.rs` |
| 输出（复制 / 保存 / 贴图） | `output.rs`、`output_settings.rs`、`clipboard_data.rs`、`pinned.rs` |
| 长截图 | `scroll_capture.rs`、`scroll_stitch.rs`、`scroll_input.rs` |
| 录像与按键显示 | `recorder.rs`、`recording_ui.rs`、`gif_export.rs`、`input_overlay.rs` |
| 文字识别与翻译 | `ocr.rs`、`paddle_ocr.rs`、`ocr_pack.rs`、`codes.rs`、`pin_text.rs`、`translate.rs`、`pin_translate.rs` |
| 识图 | `image_search.rs`、`image_search_settings.rs` |
| 设置、主题、图标 | `settings.rs`、`theme.rs`、`icons.rs`、`icons/lucide.rs` |
| 安装与更新 | `setup.rs`、`update.rs`、`update_ui.rs` |

每个模块的职责、界面约定和调试手段见 [AGENTS.md](AGENTS.md)，架构取舍见 [docs/technical-plan.md](docs/technical-plan.md)。几个常见扩展点：

- **新增翻译服务**：在 `translate.rs` 加服务商，在 `translate_settings.rs` 加对应输入框；测试用 `http::tests::serve` 起本地 mock。
- **新增工具栏按钮**：`toolbar.rs` 的按钮表和 `GROUP_STARTS` 分组，`hotkey.rs` 的绑定和默认键；布局变化要同步更新 `toolbar.rs`、`annotation_options.rs` 和 `selection/tests/text.rs` 里的测试。
- **图标**：SVG 放 `tools/icons/`，运行 `python tools/svg2rust.py tools/icons <lucide 版本> <名字...> > src/icons/lucide.rs` 重新生成。
- **配色**：颜色一律从 `theme::palette()` 取，不在界面代码里写死；需要新颜色就加 palette 字段。

## 许可

[MIT](LICENSE)。界面图标基于 [Lucide](https://lucide.dev/)（ISC，[licenses/lucide.txt](licenses/lucide.txt)）；二维码 / 条形码识别用 [rxing](https://github.com/rxing-core/rxing)（Apache-2.0，[licenses/rxing.txt](licenses/rxing.txt)）；另用了 [serde_json](https://github.com/serde-rs/json) 和 [gif](https://github.com/image-rs/image-gif)（MIT / Apache-2.0）。长截图、马赛克、序号和鼠标按键图标为自绘。
