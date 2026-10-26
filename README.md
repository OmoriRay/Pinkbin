<div align="center">

<img src="apps/desktop/src-tauri/icons/128x128.png" alt="Pinkbin" width="80">

# Pinkbin

AI 对话式空间分析 + 传统规则清理，Windows 便携自用版。

[下载便携版](https://github.com/OmoriRay/Pinkbin/releases/latest) · [使用说明](docs/使用说明.md) · [更新记录](docs/CHANGELOG.md) · [English](README_EN.md)

</div>

## 这个版本做了什么

基于 [cccyd2003-qwq/pinkbin](https://github.com/cccyd2003-qwq/pinkbin) 修改，保留原项目的粉色界面和 AI 对话式空间分析。当前版本 **v0.4.0**，面向 Windows 10 / 11 x64。

- 移除右侧 Studio，保留文件树与 AI 对话作为主界面。
- AI 无须扫描即可对话；扫描后可以结合文件元数据分析，支持兼容接口和本地 Ollama。
- 加入固定本地磁盘的全盘检查，以及经过规则限定的常用缓存检查。
- 清理结果与历史集中展示；分类全选跨分页，支持复制文件信息和已选文件信息。
- 扫描由后台保留，界面刷新后重新连接；退出程序后任务不保留。
- 窗口尺寸在显示前恢复；AI 回复显示耗时，提供连接测试与网络连接复用。
- 程序自己的设置、窗口状态、记录和 WebView 数据放在 exe 同级的 `PinkbinData`。

## 下载和使用

到 [Releases](https://github.com/OmoriRay/Pinkbin/releases/latest) 下载 `Pinkbin_0.4.0.exe`，放入一个有写入权限的文件夹，双击使用，无须安装包。Windows 需要 WebView2 Runtime；多数 Windows 10 / 11 设备已经具备。

首次使用会在 exe 旁创建 `PinkbinData`。已有旧版 AppData 设置时会尝试导入，原数据保留。以后移动软件，请将 **exe 与 PinkbinData 一起移动**。数据目录含有本机 AI 配置，请不要上传到仓库或分享给其他人。

使用 AI 时，在设置中填写服务商提供的 Base URL、API Key 和模型名，再点击小型测试按钮。调用第三方服务的费用及可用性由服务商决定。

## 清理范围

全盘检查遍历所选的可访问固定本地盘，列出较旧临时文件、崩溃转储、日志、安装介质和大文件。它会跳过受保护目录、链接及部分软件环境；权限或扫描限额会影响覆盖范围。

**大文件、旧文件不等于垃圾。** 全盘候选默认不选，需要人工核对后确认。规则清理不依赖 AI；执行移入系统回收站，不自动清空回收站，因此移入量不等于已经释放的空间。

普通文件分析发送路径、大小等元数据，不读取文件正文；主动附加的图片会发送给所配置的 AI 服务。对话本身不能直接执行清理。

## 从源码运行

依赖：Windows Rust MSVC 工具链、Node.js 20 或更新版本、pnpm 9，以及 Tauri 所需的 Windows 构建环境。

```powershell
pnpm install --frozen-lockfile
pnpm tauri dev
```

构建便携 exe：

```powershell
pnpm tauri build --no-bundle
```

产物位于 `target/release/pinkbin.exe`，可以自行重命名。测试使用 `cargo test --workspace`，前端构建检查使用 `pnpm build`。

## 来源与许可证

原项目：[cccyd2003-qwq/pinkbin](https://github.com/cccyd2003-qwq/pinkbin)。本分支从提交 [`dc5585560f4e87a016853097573baa43699d4e3f`](https://github.com/cccyd2003-qwq/pinkbin/commit/dc5585560f4e87a016853097573baa43699d4e3f) 开始修改，并保留上游提交历史。

沿用 [MIT License](LICENSE)，保留原作者版权与许可证。本仓库是个人修改分支。
