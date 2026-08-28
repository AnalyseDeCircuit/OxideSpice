# OxideSpice 协议与实现设计

[English](protocol-design.md) · [简体中文](protocol-design.zh-CN.md)

## 范围与依据

OxideSpice 是通用 SPICE 客户端协议栈。协议与客户端 crate 不依赖 UI 框架、宿主应用
专用类型、进程 helper 或 C SPICE 客户端实现。协议依据包括：

- [SPICE 协议概览](https://www.spice-space.org/spice-protocol.html)
- [spice-protocol 字节常量](https://gitlab.freedesktop.org/spice/spice-protocol/-/blob/master/spice/protocol.h)
- [spice-protocol 枚举](https://gitlab.freedesktop.org/spice/spice-protocol/-/blob/master/spice/enums.h)
- [spice-common 字节结构定义](https://gitlab.freedesktop.org/spice/spice-common/-/blob/master/spice.proto)
- [spice-server Link 行为](https://gitlab.freedesktop.org/spice/spice/-/blob/master/server/reds.cpp)
- [QEMU SPICE 选项](https://qemu-project.gitlab.io/qemu/system/invocation.html)

## 连接流程

每个 SPICE 通道独占一条 TCP 或 TLS 流。选择 TLS 时，TLS 握手先于 SPICE Link 握手
完成；协议内部不存在 StartTLS 切换。

1. 客户端发送 16 字节小端 Link Header：`REDQ`、主版本 2、次版本 2，以及后续 Link
   消息的字节长度。
2. 客户端发送紧密排列的 Link 消息：connection ID、channel type、channel ID、公共能力
   word 数、通道能力 word 数、能力偏移，随后是两个 word 数组。新 Main 通道使用
   connection ID 0；其他通道使用 Main Init 返回的 session ID。
3. 服务端返回 Link Header 和 Link Reply。固定回复长 178 字节，其中包含 162 字节 DER
   RSA 公钥和能力数组。读取或分配前会检查所有计数、偏移、加法和内存上限。
4. 双方都发布 `AUTH_SELECTION` 时，客户端选择四字节机制值 1 的 Ticket 认证；如果
   调用方配置了 SASL 且 peer 发布相应能力，则选择 SASL。SASL 支持 GSSAPI、
   SCRAM-SHA-512/256/1、PLAIN 和 LOGIN。普通 TCP 需要协商 SASL security layer；TLS
   和 Unix 传输可以依赖外部保护。
5. Ticket 认证使用服务端提供的 1024 位 RSA 公钥加密以 NUL 结尾的密码。填充方式为
   PKCS#1 OAEP，使用 SHA-1、MGF1 和空 label；密文长度固定为 128 字节。随后客户端读取
   四字节 Link 结果。
6. 普通通道的帧格式在流的整个生命周期内保持不变。双方都发布 `MINI_HEADER` 时使用
   六字节 type/size 头；否则使用 18 字节完整头，其中包含 serial、type、size 和
   sub-message-list offset。解码器不会逐消息猜测帧格式。
7. Main Init 提供 session ID、显示提示、鼠标模式、Agent 状态、多媒体时间和 RAM 提示。
   客户端发送 Attach Channels，随后接收 Channels List。每个列表项是
   `(channel type, channel ID)`；列表可以重复，同一类型也可包含多个 ID。
8. 每个选中的子通道都在新传输上完成完整 Link 和认证。Display 在消费显示流量前发送
   Display Init。Inputs 以及与选中 Display 配对的 Cursor channel ID 均为可选；已发布
   通道 Link 失败时连接终止，不会静默降级。

Link 错误对相应通道是终止错误。`NEED_SECURED` 和 `NEED_UNSECURED` 表示传输策略结果，
不表示可以降低身份校验强度后静默重试。

## 通用通道行为

读取循环执行 `精确读取头 -> 校验并有界读取 body -> 分派`，不会一直读到 EOF，也不会
扩张无限 staging buffer。读取有界 body 后可以跳过未知的无状态消息；可能修改显示
缓存、表面、Agent 流控或迁移状态的未知消息会以协议错误终止通道。

- Set Ack 安装 `(generation, window)`。客户端立即发送 Ack Sync，并在每消费一个窗口后
  发送 Ack。window 为零时关闭周期性 Ack。
- Ping 使用相同的 ID 和时间戳回复 Pong。
- Wait For Channels 使用固定 session registry，以 `(channel type, channel ID)` 为键。
  每个所有者只在完成消息状态转换后发布进度；等待方休眠到目标单调 serial，并保持
  可取消。Mini header 使用本地推导的接收 serial；完整头 serial 可以跳号但不能回退。
- 断开过程停止接收新工作、记录远端原因、执行有界协作清理并关闭通道。
- 完整头 sub-message list 与迷你头 `SPICE_MSG_LIST` envelope 会在分发前完成整体校验。
  sub-message 按列表顺序先于主消息执行；ACK 计数和跨通道进度只按所属 wire envelope
  前进一次。所有偏移仍相对于当前有界 body，不会被当作宿主指针。

## 能力矩阵

客户端发布的能力始终对应已实现行为。能够解析一个能力不等于支持该能力。

| 区域 | 协议能力与依赖 | 交付策略 |
| --- | --- | --- |
| Common | 0 auth selection、1 Ticket、2 SASL、3 mini header | 始终发布 auth selection、Ticket 和 mini header。只有调用方提供 SASL 策略时才发布 SASL。 |
| Main | 0 semi-seamless migration、1 name/UUID、2 agent connected tokens、3 seamless migration | 发布全部四项。目标通道使用源 session ID 预连接，按 migration generation 排队，只在通道迁移或 Main 迁移完成时启用。 |
| Display | 0 sized stream、1 monitors、2 composite、3 A8 surface、4 stream report、5 LZ4、6 preferred compression、7 GL scanout、8 multi-codec、9 MJPEG、10 VP8、11 H.264、12 preferred video codec、13 VP9、14 H.265、15 GL scanout 2 | 发布 Composite、A8、LZ4、stream 和 codec 路径。只有能通过 `SCM_RIGHTS` 接收 DMA-BUF 描述符的 Linux Unix 套接字端点才发布 GL scanout。 |
| Cursor | 无通道专用能力位；独立 shape cache 和 set/move/hide/trail 消息 | 支持 alpha、mono、color4、color8、color16、color24、color32、缓存与失效、reset/init 顺序和最新完整状态。静态 RGBA 硬件光标不能表达 framebuffer inversion，因此 destination-invert 像素使用固定棋盘回退。 |
| Inputs | bit 0 key scancode；旧式按键和相对/绝对鼠标消息始终可用 | 支持协商后的原始扫描码、旧式按键、两种已确认鼠标模式、离散按钮、modifier 状态和 motion ACK 流控。 |
| Agent | 通过 Main 通道传输，拥有独立能力 word、2,048 字节数据分片和 token 流控 | 支持稀疏显示器布局、带选择区的多格式剪贴板、WebDAV 文件列表、双向音量、图形设备映射，以及按代次隔离的详细文件传输错误。 |
| Playback | CELT、volume、latency、Opus；原始 S16 无 codec bit | 发布 Opus、volume 和 latency；使用随包 libopus 将数据解码为有界交错 S16LE，并发布最新 gain、mute 和 latency。保留原始模式，不发布过时 CELT。 |
| Record | CELT、volume、Opus；支持原始 S16 | 发布 Opus 和 volume。只有服务端及其请求的双声道采样率允许时才选择 Opus；精确缓冲 480 sample frame，通过随包 libopus 编码，并发布最新 gain 和 mute。保留原始采集。 |
| USB redirection | SpiceVMC 帧和可选 LZ4；负载属于独立 usbredir 协议 | 发布 SpiceVMC LZ4，并提供有界可靠原始流，由唯一后端持有 Hello 和设备状态。helper 使用独立 event worker 集成动态 usbredirhost 和 libusb；协议 crate 保留纯 Rust 数据包解析器。 |
| Smartcard | VSC 消息，由 spice-server 条件提供 | 连接并监管带类型的有界 VSC 消息。helper 集成 PC/SC 读卡器发现、ReaderAdd/ATR、APDU、Flush 和错误回复。 |
| Port | SpiceVMC 字节以及 port name/open/close/break 状态 | 作为有界双向字节流暴露，不解释应用层协议。LZ4 协商成功且能缩小字节数时在两个方向使用。 |
| WebDAV | `org.spice-space.webdav.0` 对应的 Port 子类型 | 客户端保持不透明有界字节流。helper 将其桥接到 HTTP/1 WebDAV handler，只映射调用方授权的根目录，并显式选择只读或读写方法。 |
| 多显示器 | Display Monitors Config 和 Agent monitor configuration；允许多个 Display channel ID | 身份为 `(display channel ID, monitor ID)`。期望布局会合并，并在 Agent 重连后重放；不支持的物理尺寸字段会省略。 |
| 剪贴板 | Agent on-demand、selection、serial、re-grab 和 maximum-size | UTF-8、PNG、BMP、TIFF、JPEG 和 file-list 共享一条 8 MiB 路径。文件列表校验 `copy`/`cut` 和以 NUL 结尾的 WebDAV 绝对 UTF-8 路径。 |
| 文件传输 | Agent Start/Status/Data 和 token 流控；剪贴板文件列表改用 WebDAV | 出站传输只接受 basename，最多八个活动身份，每块 64 KiB。每个所有者同时只有一个已完整分片的 chunk 在途，支持终止状态和显式取消。文件系统 I/O 位于客户端 crate 之外。 |

### 图像与视频流格式

经典 Canvas 路径实现 302 至 313 消息。共享 renderer 处理内联矩形 clip、定位 QMask
位图、纯色与重复 pattern brush、nearest 或 interpolate 缩放、二元 ROP descriptor 以及
任意 ROP3 真值表。Stroke 对有界 fixed28.4 path 实现 cosmetic line、dash、close 和
Bezier；Text 对有界 A1/A4/A8 光栅 glyph 进行绘制。客户端发布零字节 pixmap cache，
因此 cache-reference image type 不属于协商后的消息流；palette 与 GLZ cache 仍分别有界。

| 格式 | 协商语义 | 策略 |
| --- | --- | --- |
| Bitmap | 基础图像类型，无 opt-out 能力 | 支持经过检查的直接色 RGB555、BGR24、xRGB32、ARGB32，以及大端 1/4 位和 8 位索引更新。内联与缓存调色板均有上限，并遵守单项/全部失效。 |
| LZ / GLZ | 基础图像类型，无 opt-out 能力；GLZ 有共享历史及跨通道/缓存顺序 | Rust LZ 1.1 支持全部头格式。GLZ 支持 RGB16/24/32 和分离 alpha 的 RGBA，使用有界 session dictionary、跨图像引用、乱序多 Display 等待和连续 ID 淘汰。`GLZ_RGB` 外层图像不会产生 palette GLZ。 |
| zlib-GLZ | 基础图像类型 | 使用 `miniz_oxide` 有界流式实现，校验声明的 GLZ 长度、checksum、64 KiB 输出块间取消、短解压、超长输出和尾随压缩字节，不依赖 `libz-sys`。 |
| LZ4 | 仅在发布 Display bit 5 后服务端可发送 | 发布并解码经过检查、按字典链接的 row block，校验尺寸和工作内存，支持取消、RGB16/24/32 与 RGBA 转换。`lz4_flex` 为 safe Rust，不依赖 `-sys`。 |
| JPEG / JPEG-alpha | 基础图像类型，无 opt-out 能力 | baseline JPEG 使用纯 Rust `zune-jpeg`，严格检查尺寸并支持协作取消。progressive Huffman DCT 使用有界纯 Rust `jpeg-decoder`，原地将 RGB 扩为 RGBA。JPEG-alpha 在合并 alpha 前校验其 LZ `XXXA` plane。 |
| QUIC image | SPICE SFALIC 系列图像 codec，与 IETF QUIC 传输协议无关 | Rust 实现支持 RGB16、RGB24、RGB32 和 RGBA，包括有界 Golomb escape code、自适应模型、2,048 像素模型切换、跨行预测与 MEL run。SPICE canvas 显示路径不接受 grayscale，因此拒绝该格式。 |
| Video streams | Multi-codec 会改变能力语义；缺少该能力时服务端会假定支持旧式 MJPEG | 支持 sized/fixed geometry、clip 替换、销毁、report window 和首选 codec 顺序。最多 16 个流，每个流独占一个单命令 decoder worker，使原生或 CPU 密集解码不阻塞网络任务。 |
| GL scanout | Unix 描述符和 DMA-BUF 路径 | Linux Unix 套接字通过有界 `SCM_RIGHTS` 接收单平面或多平面描述符。宿主获得 owned frame，完成或释放后才发送 `GL_DRAW_DONE`。TCP/TLS 会话不发布这些能力。 |

保留的 QUIC 测试使用 `spice-common` commit
`71e45706981973014eaab3d4b533d35d79e19ffa` 逐字节生成并解码的流：RGB32 1x1、RGB24
跨行预测、RGB32 MEL run、RGBA 分离 alpha，以及 RGB16 5-bpc 跨行样例。生成的 RGB32
4097x2 样例也在 2,048 和 4,096 模型边界上通过精确像素比较，其 SHA-256 为
`e9a5664fe283beee3b2ef8f62241e77f2dea3d55459a2d4b6f127e27e5d573fe`。官方编码器只用于
生成研究向量，不由 Cargo 编译，也不会链接到 OxideSpice。

基础 QEMU 互操作配置使用
`image-compression=off,jpeg-wan-compression=never,zlib-glz-wan-compression=never,streaming-video=off`。
通过该配置证明基础纵向路径，不代表所有生产显示配置。

## 状态机与所有权

session supervisor 持有连接 attempt generation、Main owner、子通道 registry、取消源和
所有任务句柄。每个通道任务独占自己的传输，不在 mutex 后共享 socket。

```text
Idle
  -> ConnectingMain
  -> LinkingMain
  -> AuthenticatingMain
  -> AwaitingMainInit
  -> DiscoveringChannels
  -> Running
       -> PreparingMigration -> ConnectingTarget -> Switching -> Running
       -> Reconnecting -> ConnectingMain
  -> Closing
  -> Closed
```

任何转换都可能携带类型化终止原因进入 `Failed`。新连接或迁移 attempt 会增加
generation；所有权转移后会忽略旧 generation 的事件。

每个通道独立经过 `Transport -> Link -> Auth -> Active -> Draining -> Closed`。取消会停止
接收命令、关闭通道传输并等待任务结束。supervisor 对取消的监听独立于 socket 读写。
每个通道有两秒协作清理窗口；仍然阻塞的任务会被 abort 并继续等待回收。
`Session::shutdown` 只有在所有通道任务回收后才完成。

迁移由 session 持有。向源端确认前，目标 Main 和全部现有子通道身份会使用源 session ID
完成 Link。Display 和 Record 目标传输在等待期间发送必要初始化。每通道
`MIGRATE_FLUSH_MARK` 和不透明 `MIGRATE_DATA` 先于替换流量排序；即使传输局部 serial
重新开始，有效接收 serial 仍保持单调。目标 ACK 后，无缝迁移保留 surface、cache、
stream、Agent 状态和设备协议 generation；目标 NACK 则回退到半无缝完成路径。半无缝
启用会重置服务端持有的 display、input、cursor、audio、Port、Agent、USBredir 和
Smartcard 状态。排队设备输入带有 transport generation，防止宿主解析器拼接不同服务
端的字节。取消会先使目标 generation 失效，再阻止任何排队替换启用。

旧式 `MIGRATE_SWITCH_HOST` 使用 connection ID 0 重新连接 Main，初始化新 session，要求
受管理的子通道拓扑一致，更新可观察 session ID，并立即替换全部旧传输，无需等待源端
EOF。

guest Agent 从属于 Main owner，状态为 `Disconnected -> Negotiating -> Ready`。
`AGENT_CONNECTED_TOKENS` 替换出站 credit，后续 Agent Token 消息累加 credit。每个接收
token 对应一个 Main AgentData 分片，而不是一个逻辑 Agent 消息。断开或重连会重置部分
重组、剪贴板 serial、待读取、保留 credit 和文件传输。期望显示器布局与本地剪贴板
所有权会在新的 Agent generation 中重放；文件传输不会恢复。

## 有界资源

所有上限都使用具名配置值、保守默认值和由协议推导的校验。

- Link body 上限为 4 KiB，与 spice-server 行为一致。
- capability 数量在乘法和分配前检查。
- 普通消息 body、sub-message 数、图像尺寸、surface 字节、decoder 输出、cache 字节、
  Agent 消息、剪贴板项目、文件传输窗口和 Port 字节分别设置独立上限。
- Agent 逻辑消息上限为 16 MiB，直接重组成声明的 body。复用 completion list 和
  2,048 字节出站分片 buffer，避免每个 Main AgentData 分片产生新分配。剪贴板类型数组
  最多 64 项。
- Agent 初始拥有十个接收分片 credit。可靠的宿主剪贴板请求会保留对应 credit，直到
  event lease 释放，从而通过协议背压避免无限事件队列。credit generation 切换和 pending
  credit 重置保持原子性，断开 Agent 的迟到事件不能给新流补充 credit。
- 出站文件传输元数据只包含经过检查的 guest basename 和声明大小，不包含宿主路径。
  chunk 上限为 64 KiB；当前逻辑消息完整分片到 Main 之前，streaming owner 不能提交下
  一个 chunk。Agent 断开会将全部活动传输标为终止；旧 generation 命令不能进入替换流。
- 原始 Playback 最多接受 32 个交错通道、384 kHz 采样率和每包 256 KiB。网络任务使用
  16 项非阻塞队列；溢出时丢弃实时包并把下一交付包标记为 discontinuous，不阻塞 SPICE
  控制流量。
- 原始 Record 在其他客户端 Record 消息之前发送 Mode。只有服务端 Start 后、PCM Data
  前才发送 Start Mark。采集提交上限为 256 KiB，按请求的交错 frame width 校验，并通过
  16 项命令队列串行化。宿主可使用 session 单调时间戳，也可显式提供采集后端时间戳。
- Port name、指针偏移、终止符、事件和 256 KiB Data 消息在转移所有权前检查。宿主写入
  和 break 事件使用 16 项有界命令队列。Port 与 WebDAV 应用字节保持不透明，客户端
  crate 不持有文件系统。
- usbredir 传输 chunk 上限为 1 MiB，使用 16 项可靠队列。客户端不会注入竞争 Hello；
  选中后端持有完整 usbredir 流。helper 的有界 worker 驱动 usbredirhost/libusb；协议
  crate 为 Rust 后端保留经过检查的 Hello、32/64 位 header 和 packet 解析。
- 网络任务不会等待 UI 展示。控制与图形交付使用独立有界路径。图形路径饱和时，在
  语义允许处合并脏区域，否则请求 base-frame 恢复。按键/按钮边沿和状态转换不会丢失。
- 有序输入边沿使用 128 项队列，同时支持异步背压和显式非阻塞失败。绝对位置使用
  latest-only slot，相对 delta 会累加。最多允许两个协议 ACK bunch 在途；按钮边沿先
  flush 当前指针状态，避免点击落在旧坐标。
- 光标图像检查尺寸，使用 256 项协议 cache 和 4 MiB live byte budget。失效后字节费用
  跟随宿主持有的 `Arc` shape，直到最后一个 owner 释放图像才归还。
- 每个 Display 通道拥有 256 项、256 KiB palette cache。palette pointer 只能在当前
  有界消息内解析；direct-color 图像不能夹带 palette union flag，cache miss 会终止通道，
  不会使用猜测颜色渲染。
- SPICE LZ 的输入、尺寸、stride、输出字节、literal、match length 和 back-reference
  distance 在修改内存前检查。解码运行在 socket 任务外，并在较长扩展、复制和像素转换
  中轮询 session 取消。所有 Display 共享一个 cancellation-aware decode slot；压缩输入
  在取得 slot 后才复制，因此单图像上限同时也是 session 的瞬时内存上限。
- 每个 Display 发布相同 GLZ dictionary ID 和 16 MiB window。缺少旧图像时，decoder 会
  在等待其他 Display 发布之前释放共享 decode slot，避免 dependency inversion。淘汰只
  跨过连续全局 GLZ ID，其他传输的早到图像不能越过未解析 ID gap 丢弃数据。
- zlib 包装 GLZ 使用同一 session decode slot。wrapper 会拒绝零值或超限声明输出、短
  解压、超出声明长度、checksum 失败和尾随压缩字节，之后才允许内部 GLZ 图像进入字典。
- JPEG 输入在分配前与可选 alpha 流分离。只有 baseline frame 进入纯 Rust decoder；
  descriptor 尺寸、配置尺寸、精确 RGBA 输出和取消均会检查。JPEG-alpha 必须携带同尺寸
  且行方向一致的 `XXXA` LZ plane，像素才可进入 surface。
- QUIC 输入使用经过检查的小端 word reader，在每个 word 内按最高有效位优先消费。分配
  前匹配 header 与 descriptor 尺寸；索引或复制前限制 Golomb escape value 和 MEL run。
  decoder 输出直接归一化为 top-down RGBA，不产生第二次完整帧转换复制。
- 帧负载保留在 owned surface storage。通知携带 generation、surface identity 和 dirty
  元数据，不复制完整帧。latest-only notifier 替换旧 dirty 通知时，将新通知标为需要
  full refresh。
- Display reset、surface recreation、migration 和 reconnect 会增加 graphics epoch，避免
  过期更新修改新帧。
- 默认最多连接 16 条 Display 传输，共享 256 MiB live surface budget。每个 frame 和
  topology 事件携带 `(connection generation, Display channel ID, graphics epoch)`，因此
  不同通道中的相同 surface ID 不会冲突。

## 错误模型

错误保留稳定分类和结构化上下文，同时不携带凭据或帧数据：

- 配置错误和不支持的功能；
- DNS、网络和传输超时；
- TLS 身份或策略；
- Ticket 或 SASL 认证；
- Link 协商和远端 Link 结果；
- 错误字节数据或不支持的有状态消息；
- decoder 失败或不支持的图像/视频流格式；
- 本地资源上限；
- 远端断开；
- 迁移或重连 attempt；
- 本地取消和关闭超时；
- 内部任务终止。

不支持的有状态 Display 数据属于终止协议/功能错误。静默跳过后继续运行可能产生看似
合理但已经损坏的 framebuffer。

## Crate 边界

workspace 包含五个 crate：

- `oxide-spice-protocol`：依赖精简的字节类型、常量、安全解析器和编码器，不包含 I/O
  运行时、UI、文件系统或 codec 实现。
- `oxide-spice-codecs`：不依赖运行时的有界纯 Rust 图像解码。持有 SPICE LZ 1.1、GLZ、
  zlib wrapper、baseline JPEG 和 SPICE QUIC，并在不依赖 Tokio 或客户端状态的情况下
  支持协作取消。
- `oxide-spice-client`：异步传输、Link 认证、session/channel 所有权、ACK、取消状态、
  surface 和有界交付。
- `oxide-spice-helper-protocol`：纯 Rust 的版本化 helper IPC schema、有界 JSON/二进制
  编解码、凭据前 Hello 协商和制品元数据类型。helper 与外部宿主适配器共同使用该 crate。
- `oxide-spice-helper`：独立有界 stdio 进程，以及 USB、PC/SC 和本地 WebDAV 文件系统
  映射。只暴露 SPICE 语义，不依赖 UI 框架或宿主应用专用类型。宿主先取得已发布通道
  ID，再授予目录或设备权限；原生设备发现保留在 helper 进程内。

## 依赖策略

协议、Ticket、LZ、GLZ、zlib、LZ4、JPEG、QUIC 和 H.265 实现使用 Rust。baseline JPEG
使用 `zune-jpeg`，progressive JPEG 使用 `jpeg-decoder`，H.265 使用 `rust_h265`。Tokio
和 WebDAV 本地文件系统后端使用 `libc` 等 Rust OS binding，但不会链接 C SPICE 客户端。

Composite 渲染使用 MIT 许可证的 `pixman`/`pixman-sys` binding。该原生光栅边界实现
Draw Composite 的 operation、transform、filter、repeat、component-alpha、clip 和 A8
语义，不链接 SPICE 客户端库。正式 helper 制品会构建固定版本的 Pixman 源码并静态
链接。Unix 描述符接收使用 safe `rustix` API，所有项目 crate 均保持
`unsafe_code = "forbid"`。

客户端通过 `composite-pixman`、`audio-opus`、`sasl-gssapi`、`video-h264`、
`video-h265` 和 `video-vpx` 功能选择原生光栅、认证与媒体边界。完整客户端默认启用
它们，也可逐项关闭。能力发布从实际编译功能推导；被关闭的 codec 或 Composite
backend 不会向服务端发布。

SASL 密码机制由 Rust `rsasl` 提供。Rust 实现的 RFC 4752 状态机通过 `cross-krb5`
调用平台能力：Linux 经 `libgssapi-sys` 使用 MIT 或 Heimdal GSSAPI，正式 Linux 制品
携带固定版本的 MIT Kerberos；macOS 使用系统 GSS framework；Windows 直接使用原生
SSPI Kerberos，不加载 libgssapi。原生边界只负责 Kerberos context、wrap 和 unwrap；
SPICE SASL 帧、层选择、边界检查和 record frame 仍由 Rust 客户端持有。

TLS 是显式的 `oxide-spice-client/tls-ring` 功能。它关闭 `tokio-rustls` 默认功能，选择
`ring` 和 TLS 1.2。`ring` 会编译随包 C 与汇编代码；这些原生代码只位于传输密码学边界，
不会进入协议或 decoder crate。全 Rust RustCrypto provider 仍处于 pre-production，未被
采用。调用方持有 rustls certificate/verifier 配置，因此启用 TLS 不会削弱身份校验。
迁移默认验证目标 hostname。只有显式 `MigrationTlsPolicy` 返回调用方针对目标设置的
server name 和 rustls 配置时，才接受源端提供的 certificate subject；缺少该策略时会在
连接前终止迁移。

helper 会转发上述开关，并分别用 `tls-ring`、`usbredir`、`smartcard` 和 `webdav` 控制
宿主集成。IPC 结构不随功能组合变化；请求构建时省略的后端会返回明确的操作错误。

原生依赖保持隔离并显式披露。`opus 0.4.0` 使用 `opusic-sys`，通过 CMake 编译采用 BSD
许可证、随包提供且可能包含平台汇编的 libopus。helper 使用 `usbredirhost 0.4.1`，间接
动态链接 `usbredirparser-sys` 和系统 usbredir/libusb，此路径由 `usbredir` 功能控制；
`pcsc 2.9.0` 通过 `pcsc-sys` 使用平台 PC/SC 服务，此路径由 `smartcard` 功能控制。Linux
制品会构建并携带固定版本的 PCSC-Lite 客户端库与 real delegate 库，pcscd socket 和
daemon 仍由系统提供；macOS 与 Windows 使用平台 PC/SC 实现。
usbredir/libusb 是动态链接的 LGPL 库，并保留自身分发条款。
`oxide-spice-protocol` 不包含这些依赖。Display 和 SpiceVMC LZ4 使用 safe Rust
`lz4_flex`，不使用 `lz4-sys`。

VP8/VP9 使用 `vpx-rs -> env-libvpx-sys`；正式制品会构建并静态链接固定版本、采用 BSD
许可证的 libvpx，其 Rust 构建步骤使用 bindgen/libclang。H.264 使用
`openh264 -> openh264-sys2`，编译
随包提供、采用 BSD 许可证的 OpenH264 C++ 和汇编。两者都不会进入
`oxide-spice-protocol`。helper 的 Apache-2.0 `dav-server` 本地文件系统功能使用 Rust
`libc` crate 进行平台文件系统调用，不会随包提供原生 WebDAV 实现。

`native/dependencies.toml` 记录原生归档的版本、下载地址、散列、许可证和链接策略。
六目标制品工作流会在解压前校验每个归档，要求完整 helper 能力契约，审计动态依赖，
修正相对运行时路径，并打包元数据、许可证文本、第三方声明和 CycloneDX SBOM。
usbredir/libusb 继续采用可替换的动态链接方式，以保留 LGPL 要求的替换能力。
