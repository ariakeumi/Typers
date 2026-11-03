# Typers

<div align="center">

![Typers 项目横幅](assets/icon/velotype-banner.png)

**基于 Rust 和 GPUI 的原生 Markdown 编辑器。**

[编辑器展示](assets/showcase/showcase.md) · [二开界面与快捷键](docs/typora-native.md) · [上游 Velotype](https://github.com/manyougz/velotype)

</div>

Typers 是 [Velotype](https://github.com/manyougz/velotype) 的 fork，在上游原生块编辑器的基础上，以 Typora 风格的写作体验为方向进行二次开发。感谢 Velotype 及其贡献者的工作。当前 fork 以本地仓库中的上游 `v0.7.8` 为基础；Typers 与上游是各自独立演进的项目。

编辑器使用 Rust 和 [GPUI](https://gpui.rs/) 构建，支持即时渲染编辑和 Markdown 源码编辑。它保留上游的块树、Markdown 解析与序列化架构，编辑界面不依赖 WebView 或 Electron。

## 二次开发内容

相对于上述上游基线，本仓库主要做了以下调整：

- **写作界面**：参考 Typora 的 Github 主题和布局，调整正文、标题、引用、列表和代码块样式，内置 Open Sans 字体；重新设计偏好设置、菜单和窗口细节。
- **写作与导航**：完善文件树、文档列表、大纲和侧栏交互，增加专注、打字机、只读模式及窗口独立缩放，并提供字数和阅读时长统计。
- **查找与编辑**：支持正则查找、捕获组替换、匹配高亮，以及跨块键盘选择；改进渲染与源码模式切换时的选区、撤销和编辑行为。
- **Markdown 兼容与稳定性**：改进链接、图片、格式化文本及源码映射等处理，修复中文和 emoji 等多字节文本选区可能触发的崩溃。
- **性能**：优化光标刷新、共享引用更新、Markdown 序列化和滚动时的可见区域渲染。测试条件与结果见[编辑性能记录](docs/markdown-edit-performance.md)。
- **应用标识和打包**：使用 Typers 名称、图标和应用 ID，并调整跨平台发布及 macOS 打包配置。Cargo 包和可执行文件目前仍名为 `velotype`。

具体实现、快捷键和已知边界见 [Typora 风格原生界面说明](docs/typora-native.md)，选区问题的分析见[崩溃修复记录](docs/selection-crash.md)。二开仍在持续完善，不能保证与 Typora 的全部功能或行为一致。

## 功能

- 即时渲染和源码两种编辑模式，以 Markdown 文件保存内容。
- 支持标题、列表、任务列表、引用、表格、代码块、链接、图片、脚注等常见语法，以及公式、Mermaid 和部分 HTML 内容。
- 提供工作区、文件树和大纲，支持自定义主题与界面语言。
- 支持导出 HTML 和 PDF。HTML 使用当前主题生成样式；PDF 沿用基于外部 Chromium 的打印流程。
- 面向 macOS、Windows 和 Linux；各平台的构建依赖和运行环境有所不同。

部分 Markdown 内容在保存时会被规范化；对于复杂文档，建议先使用副本检查编辑和保存结果。项目仍在开发中，输入法、语法及平台体验可能有待完善。

## 获取与运行

可在 [Typers 发布页面](https://github.com/ariakeumi/Typers/releases)查看本 fork 的构建。上游 [Velotype 发布页面](https://github.com/manyougz/velotype/releases)提供的是上游版本，不包含这里描述的二次开发改动。

### 从源码构建

需要 Git、Rust 1.92 或更新版本、Cargo，以及 GPUI 和系统工具链所需的平台原生依赖。Linux 的依赖列表可参考[发布工作流](.github/workflows/build-release.yml)。

```bash
git clone https://github.com/ariakeumi/Typers.git
cd Typers
cargo run --release
```

也可以只构建，然后运行生成的可执行文件：

```bash
cargo build --release
# macOS / Linux
target/release/velotype
# Windows: target\release\velotype.exe
```

macOS 用户可通过 `scripts/create_macos_app_dist.sh` 构建 `dist/Typers.app`。该脚本会先清理 `dist` 目录，请先保存其中需要保留的文件。

## 快捷键与配置

macOS 默认使用 `⌘/` 切换渲染与源码模式，`⌘F` 查找，`⌘⌥F` 替换，`⇧⌘L` 显示或隐藏侧栏。更多快捷键及模式说明见[界面文档](docs/typora-native.md)。

主题和语言包分别管理，可在应用的主题或语言菜单中导入 `.json` / `.jsonc` 文件：

- [自定义主题示例](assets/custom-theme.example.jsonc)
- [自定义语言示例](assets/custom-language.example.jsonc)

主题中未填写的字段继承所指定的内置基础主题；语言包中缺失的文案回退到英文。可设置 `VELOTYPE_CONFIG_DIR` 指向独立配置目录，以隔离现有配置。

## 开发与反馈

欢迎在 [Typers 仓库](https://github.com/ariakeumi/Typers)提交问题或改进。报告问题时，请尽量附上系统版本、复现步骤和可分享的最小 Markdown 示例。

```bash
cargo test
cargo fmt --check
```

更多验证和问题记录位于 [`docs/`](docs/)。

## 许可证与致谢

本项目继承上游的 [Apache License 2.0](LICENSE-APACHE)。内置 Open Sans 字体采用 [SIL Open Font License 1.1](assets/fonts/OFL.txt)。感谢 [Velotype](https://github.com/manyougz/velotype) 项目、GPUI 及所有依赖项目的贡献者。
