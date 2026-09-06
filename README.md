# ruozhi

桌面版小智（xiaozhi）语音客户端：按住**说话键**对麦克风说话，松开后自动发送，
AI 回复通过本机喇叭播放。窗口用 [fenster](https://github.com/zserge/fenster) 逐像素绘制，
Siri 风格动效：

| 状态 | 显示 |
|---|---|
| 待机 | 渐变呼吸圆点（蓝→紫→粉，带光晕） |
| 连接中 | 同待机，快闪 |
| 按住说话键聆听 | 圆点扩展为动效圆球，内部波纹（随麦克风电平起伏） |
| 松开发送 | 转圈（渐变圆环 + 彗星扫光） |
| 播放回复 | 圆形笑脸（随播放音量脉动） |

所有状态间通过连续参数插值过渡，动效切换平滑不生硬。

说话键：macOS 是 **Command**，Windows / Linux 是 **Ctrl**（两个平台的 meta 键
即 Win/Super，松开会弹系统菜单，不适合按住）。

协议实现参照 [xiaozhi-esp32](https://github.com/78/xiaozhi-esp32) 固件：WebSocket 通道
（JSON 文本帧 + 裸 Opus 二进制帧，二进制协议 version 1），16 kHz 单声道 60 ms 帧上行，
按服务端 hello 协商的采样率（通常 24 kHz）解码播放。

## 构建

```sh
cargo build --release
```

| 平台 | 依赖 | 产物 |
|---|---|---|
| macOS | Xcode CLT（C 编译器与 Cocoa framework） | `target/release/ruozhi`，用 `scripts/make_app.sh` 打包成 `.app`（含图标） |
| Windows | VS Build Tools（MSVC + Windows SDK，rc.exe 用于嵌图标） | `target/release/ruozhi.exe`，图标/版本信息已嵌入 exe 资源 |

图标管线：`assets/icon_1024.png` 是 AI 生成原图（可随意替换），
`scripts/make_icon.sh`（macOS，sips/iconutil）重生成 `assets/ruozhi.icns`，
`scripts/make_icon.py`（任意平台，需 Pillow）重生成 `assets/ruozhi.ico`。
生成的 `.icns`/`.ico` 直接提交在仓库里，正常构建无需重跑。

fenster 头文件已 vendor 在 `c/fenster.h`，相对上游打了两个补丁（见文件头注释）：
macOS 事件循环响应 FlagsChanged（否则单独按 Command 收不到修饰键变化）；
Windows 用 AdjustWindowRect 保证客户区精确等于 240x240（上游把缓冲区尺寸当
外框尺寸用，画面会被标题栏裁掉一角）。无外部路径依赖。

## MCP 工具（AI 可调用）

`tools/list` 向服务器声明：`self.get_device_status`（音量/电池/网络/屏幕亮度）、
`self.audio_speaker.set_volume`、`self.get_device_info`（系统版本/CPU/内存/GPU）、
`self.screen.set_brightness`（实际调亮度：macOS 走 CoreDisplay，Windows 走
DDC/CI + WMI 兜底，Linux/BSD 走 sysfs + gdbus 兜底）、`self.reboot`（重启 app）。
对话中说"把音量调小""现在电量多少""屏幕调暗点"等，AI 会自行调用。

启用**服务器端回声消除**（二进制协议 v2 帧时间戳）时加 `--server-aec`，或配置文件
`server_aec = true`。

## 配置（默认零配置）

无需任何配置文件即可运行——连接设置（`url`/`token`）像真实设备一样在启动时从
`https://api.tenclass.net/xiaozhi/ota/` 自动获取。设备身份由本机硬件决定，每台机器
固定不变：

- `device_id` = 本机 MAC 地址（`aa:bb:cc:dd:ee:ff`，对应固件的 Device-Id）
- `client_id` = MAC 哈希派生的 UUIDv4（对应固件首次生成后永久保存的 uuid）

需要固定成特定设备（比如复刻你手里那块 ESP32 的身份）或对接自建
`xiaozhi-esp32-server` 时，才**手工**创建配置文件
（macOS/Linux：`~/.config/ruozhi/config.toml`；Windows：`%APPDATA%\ruozhi\config.toml`；
也可用 `--device-id` / `--client-id` / `--url` / `--token` 命令行临时覆盖）：

```toml
log_file = "/tmp/ruozhi.log"      # 可选：日志路径（默认随平台，见「日志」一节）
device_id = "aa:bb:cc:dd:ee:ff"   # 可选：固定设备 id
client_id = "…uuid…"              # 可选：固定 client id
url      = "ws://your-server/..." # 可选：自建服务器时固定，跳过 OTA
token    = "…"                    # 可选
```

该文件是纯输入：应用**从不写入或修改**——不存在就不生成，内容坏了会告警并退回默认。

## 设备绑定

设备未绑定时，OTA 会返回 6 位激活码（日志可见）。**按住 Command 随便说句话**，
官方服务器的回复语音会播报该验证码；在 xiaozhi.me 后台输入验证码即完成绑定。

## 运行

macOS：

```sh
./scripts/make_icon.sh                           # 可选：重生成 assets/ruozhi.icns
./scripts/make_app.sh                            # 构建 target/release/ruozhi.app（含图标）
open target/release/ruozhi.app                   # 首次运行弹出麦克风授权，点允许
./target/release/ruozhi.app/Contents/MacOS/ruozhi   # 终端里跑（日志可见）
./target/release/ruozhi.app/Contents/MacOS/ruozhi --loopback      # 音频自测
./target/release/ruozhi.app/Contents/MacOS/ruozhi --wav speech.wav # 语音来自 WAV 文件
./target/release/ruozhi --demo                                    # UI 状态轮播预览
./target/release/ruozhi --render-frames /tmp/f                    # 无头导出各状态画面
```

Windows：

```sh
python scripts/make_icon.py                      # 可选：重生成 assets/ruozhi.ico
cargo build --release
./target/release/ruozhi.exe                      # 按住 Ctrl 说话，Esc 退出
./target/release/ruozhi.exe --loopback           # 音频自测
./target/release/ruozhi.exe --wav speech.wav     # 语音来自 WAV 文件
./target/release/ruozhi.exe --demo               # UI 状态轮播预览
./target/release/ruozhi.exe --render-frames out  # 无头导出各状态画面
```

Windows 麦克风隐私是按「桌面应用」整体开关的：说不出声先检查
设置 → 隐私和安全性 → 麦克风 →「允许桌面应用访问麦克风」。
双击 exe 会带一个控制台窗口（日志直接可见）；从终端跑则日志进文件。

## 麦克风权限（重要，macOS）

macOS 把麦克风授权归属到「责任进程」：`open` 启动的 `.app` 归属 ruozhi 自己；
从终端直接运行二进制（不管裸的还是在 bundle 里）归属**宿主终端 App**。被拒时
CoreAudio 不报错、静音返回——表现为按住说话只识别出「嗯」。

| 启动方式 | 权限归属 | 结果 |
|---|---|---|
| `open target/release/ruozhi.app` | ruozhi（`local.ruozhi.app`） | ✅ 正常 |
| 终端直接跑 `.../MacOS/ruozhi` | 宿主终端 | 终端有麦克风权限才可用 |
| 终端直接跑 `target/release/ruozhi` | 宿主终端 | 同上 |

坚持从终端跑且想看实时日志的话，给终端 App 授麦克风权限：
系统设置 → 隐私与安全性 → 麦克风 → 允许你的终端，**重启终端**后再跑。

其他：

- 之前点过「不允许」：`tccutil reset Microphone local.ruozhi.app` 后重新 `open`
- 应用内置诊断：启动日志标明当前权限归属；按住说话 3 秒无信号时告警并给出修复命令

## 日志

日志默认写入（追加）：macOS `/tmp/ruozhi.log`，Windows `%TEMP%\ruozhi.log`，
Linux `$TMPDIR/ruozhi.log`。路径优先级：环境变量 `RUOZHI_LOG_FILE` >
配置文件 `log_file` > 平台默认。从终端直接运行时日志同时镜像到
stdout，GUI（`open`）运行看文件：

```sh
tail -f /tmp/ruozhi.log          # 看 GUI 运行日志（macOS；Windows 是 %TEMP%\ruozhi.log）
RUST_LOG=debug open target/release/ruozhi.app   # 调试级（写进同一文件）
```

## 操作

- **按住说话键**（macOS Command / Windows·Linux Ctrl）：连接并开始聆听
  （`listen start, mode=manual`），持续上行 Opus
- **松开**：结束本句（`listen stop`），等待识别与回复，回复播完回待机
- **连接生命周期**：会话跨轮次复用（25s 间隔 keepalive ping 保活），直到 Esc 退出、
  服务器踢线或 120s 无流量；被断开后下一次按键会自动重连，无需手动操作
- **回复播放中再按说话键**：发送 `abort` 打断并立即开始新一轮
- **Esc** 退出

无麦克风权限时可用 `--wav` 注入语音文件完成验证（macOS 例如用
`say -v Tingting "你好" -o speech.wav --data-format=LEI16@16000` 生成）。

## 代码结构

```
c/fenster.h         vendor 的 fenster（FlagsChanged + AdjustWindowRect 补丁，见文件头）
c/fenster.c         #include "fenster.h"
src/fenster.rs      fenster FFI（手写声明，#[repr(C)]）；说话键定义（Command/Ctrl）
src/ui.rs           240x240 逐像素渲染：Siri 渐变圆球（呼吸点/波纹/转圈）+ 笑脸
src/state.rs        跨线程共享状态（Phase、电平、麦克风开关）
src/audio.rs        cpal 采集/播放、抗混叠 sinc 重采样、麦克风 AGC 与回复自动音量（软件实现，不动系统参数，带单测）、Opus 编解码、jitter buffer、WAV 注入
src/protocol.rs     协议线程：hello 握手、listen/abort/tts 状态机、MCP 应答、WS 收发
src/hostinfo.rs     MCP get_device_info 的本机信息（系统/CPU/内存/GPU，带缓存）
src/identity.rs     设备身份：MAC → device_id，哈希派生 UUIDv4 → client_id
src/ota.rs          OTA 引导：像真实设备一样从官方接口获取连接配置与激活码
src/config.rs       CLI 参数 + 配置文件（~/.config 或 %APPDATA%\ruozhi）
scripts/make_icon.sh 图标管线（macOS）：assets/icon_1024.png → icns
scripts/make_icon.py 图标管线（任意平台，Pillow）：assets/icon_1024.png → ico
scripts/make_app.sh 打包 ruozhi.app（图标 + 麦克风权限声明 + ad-hoc 签名）
build.rs            编译 vendor 的 fenster；链接 Cocoa/X11/user32+gdi32；Windows 下嵌图标
```
