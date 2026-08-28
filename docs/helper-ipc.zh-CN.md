# OxideSpice helper IPC

[English](helper-ipc.md) · [简体中文](helper-ipc.zh-CN.md)

`oxide-spice-helper --stdio` 持有一个 SPICE 会话。父进程向标准输入写入请求，从标准
输出读取事件；诊断信息只写入标准错误。该契约只包含 SPICE 领域类型，不依赖 UI 框架
或宿主应用的专用类型。

第一个请求必须是不含凭据的 `Hello`。helper 会先写出并刷新 `HelloAck`，输入读取器才会
继续读取 `Connect`。IPC 版本不兼容、需求重复或缺少编译能力时，进程会在读取 Ticket 或
SASL 凭据前退出。第一条请求直接发送 `Connect` 属于协议错误。输入结束或收到 `Close`
后，helper 开始有序关闭：停止原生集成、关闭全部 SPICE 通道任务、发送 `closing` 和
`disconnected` 状态、排空事件写入器，然后退出。

## 帧格式与限制

普通消息是一个 UTF-8 JSON 对象，以 `\n` 结尾。字段名和变体名使用 camelCase。单行
JSON 上限为 1 MiB。较大的帧、光标、剪贴板和 PCM 数据使用 JSON 头，后面紧接
`payloadLen` 指定数量的原始字节；负载之后没有额外分隔符。二进制负载上限为
256 MiB。读取下一行 JSON 前，读取方必须消费当前声明的全部字节。

`FrameBinary` 和 `CursorBinary` 携带紧密排列的 RGBA8 像素，负载长度必须等于
`width * height * 4`。Playback 与 Record 样本采用交错排列的有符号 16 位小端 PCM。
解码器会拒绝算术溢出、超限值、截断负载以及元数据与负载长度不一致的消息。

密码和 Ticket 不会出现在 Rust 调试输出中，所持有的内存在释放时会清零。TLS 接收由
调用方提供的 DER 信任锚和必填服务器名称。SASL 支持 GSSAPI 或密码凭据。GSSAPI 在
Linux 使用 MIT/Heimdal，在 macOS 使用系统 GSS framework，在 Windows 使用原生 SSPI
Kerberos。

## 连接顺序

要求完整 helper 的宿主先发送：

```json
{"type":"hello","hello":{"protocolVersion":1,"requiredCapabilities":["core-session","tls","sasl-password","sasl-gssapi","display-canvas","composite-pixman","audio-raw","audio-opus","video-mjpeg","video-vp8","video-vp9","video-h264","video-h265","clipboard","file-transfer","web-dav","usb-redir","smartcard","multi-display","playback","record","port"]}}
```

第一条事件是 `helloAck`。宿主必须确认 `compatible` 为 true，并检查协议版本和能力清单，
然后才能发送下面的 `Connect`。`helperVersion` 与 `target` 用于识别当前二进制；拒绝响应
会包含类型化原因。

普通 TCP 的 `Connect` 请求如下：

```json
{"type":"connect","options":{"endpoint":{"type":"tcp","host":"127.0.0.1","port":5900},"ticket":"","transportSecurity":{"type":"plain"},"sasl":null}}
```

helper 先发送 `connecting`。连接失败时发送带分类的 `error` 和 `failed`；连接成功时按
以下顺序发送事件：

1. `connected`：包含会话 ID，以及发现的 Inputs、Cursor、Agent、Playback、Record、
   Port、USBredir、Smartcard 和 WebDAV 通道能力。Inputs 还会报告是否协商了原始扫描码。
2. `serverIdentity`：包含可选的服务器名称和 UUID。
3. 值为 `connected` 的 `status`。

随后可以立即出现显示拓扑和帧缓冲事件。鼠标模式与键盘锁定键事件用于确定宿主应发送
绝对还是相对指针输入，并同步锁定键状态。帧携带连接代次和图形代次，宿主可以据此在
重置或迁移后丢弃过期工作。通常只传输脏区域，不复制完整表面；如果标准输出背压替换
了排队中的帧，新帧会改为完整主表面快照。

只有光标代次或形状 ID 变化时才发送光标形状字节。剪贴板传输显式携带选择区和格式，
不会假定数据一定是 UTF-8 文本。guest 发起的剪贴板请求带有请求 ID，宿主必须通过
`ClipboardProvideBinary` 返回该 ID。Agent 就绪状态、协商能力、guest 音量变化和图形
设备映射均使用结构化事件；反向音量更新使用 `SyncAgentAudioVolume`。Playback 包包含
流代次、序号、时间戳、格式和不连续状态。Playback 与 Record 的状态、音量、静音以及
Playback 延迟通过独立的最新状态事件发送。Record 输入在 `RecordBegin` 后通过
`RecordDataBinary` 发送，并配合相应的 `recordState` 事件。

出站 Agent 文件传输由宿主流式提供数据，不授予 helper 文件系统权限。宿主使用自己的
唯一 `transferId` 启动传输，每个 `FileTransferDataBinary` 最多发送 64 KiB，并可完成
或取消传输。`fileTransferState` 报告已接收字节数、终止状态和结构化 guest 错误。
helper 持有 guest 传输 ID 和一个四命令有界队列，同时最多运行八个传输。非空文件在
声明的最后一个分块后可以完成；零长度文件必须通过 `FileTransferFinish` 发送。

## 原生权限

原生集成只能在 `connected` 之后显式启用，因为服务端分配的通道 ID 在通道发现之前
并不存在。

普通 SPICE Port 通道不会授予本地设备或文件系统权限，因此 helper 会自动为每个普通
Port 建立有界字节桥。`portState`、`PortDataBinary` 和 `portBreak` 承载服务端到宿主的
状态与数据；`PortWriteBinary` 和 `PortBreak` 承载反向数据。读取声明的二进制负载前会
先检查 SPICE Port 的 256 KiB 消息上限。

- `ListNativeDevices` 返回 `nativeDevices`，其中包含 libusb 设备标识和 PC/SC 显示名称。
  独立的 `usbStatus` 与 `smartcardStatus` 会报告 `available` 或带原因的 `unavailable`，
  PC/SC 服务缺失不会再隐藏 USB 枚举结果。
- `StartWebDav` 授权一个已发布的 WebDAV 通道访问一个本地目录，并显式选择只读或读写
  方法。
- `StartUsbRedirection` 将一个已发布的 USBredir 通道与枚举所得的总线号、设备地址、
  vendor ID 和 product ID 配对。
- `StartSmartcardRedirection` 将一个已发布的 Smartcard 通道与枚举所得的 PC/SC 读卡器
  名称配对。

未知、重复或已经启用的通道 ID 会产生错误，不会选择默认设备。未配置的通道仍保留
传输所有权，但不会获得文件系统或物理设备权限。原生工作最多同时运行 64 个 helper
任务，并随会话一起取消。

stdio 进程不启用 GL scanout，因为标准输入输出无法携带 DMA-BUF 文件描述符。可复用
客户端仍然支持 Linux Unix 套接字上的 DMA-BUF。零复制 helper 集成需要显式的 Unix
文件描述符旁路；整数文件描述符不能作为可转移的 JSON 值编码。
