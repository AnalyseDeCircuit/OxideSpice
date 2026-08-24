# 受控 QEMU 互操作运行

[English](qemu-interoperability.md) · [简体中文](qemu-interoperability.zh-CN.md)

基础显示路径需要一个启用 SPICE 的 QEMU 构建，以及能够产生 QXL 显示流量的 guest。
该测试配置关闭压缩和视频，使首个原始位图结果可以独立观察。LZ 矩阵配置应启用 LZ，
并同时观察到 `LZ_RGB` 和 `LZ_PLT`。

测试环境使用等价于以下内容的 QEMU SPICE 选项：

```text
port=<PORT>,addr=127.0.0.1,disable-ticketing=off,password-secret=spice-ticket,\
image-compression=off,jpeg-wan-compression=never,\
zlib-glz-wan-compression=never,streaming-video=off
```

使用权限受限的文件创建 QEMU `spice-ticket` secret 对象，不要把 Ticket 写入受版本控制
的脚本或命令历史。只为探针进程导出相同的值：

```text
-object secret,id=spice-ticket,file=/path/to/restricted-ticket-file
-spice <the comma-separated options above>
```

```sh
OXIDE_SPICE_TICKET='<ticket>' \
  cargo run -p oxide-spice-client --example first_frame -- \
  127.0.0.1 <PORT> first-frame.ppm
```

Display 运行满足以下条件时通过：

1. QEMU 接受 Main 和 Display 的 Ticket 握手。
2. Main Init 提供非零会话 ID，Channels List 发布 Display。
3. 探针写出非空 PPM，尺寸符合 guest，且包含可见像素。
4. 探针无需强制超时即可退出，QEMU 报告两个客户端通道均已断开。

交互扩展还要求 QEMU 发布 Inputs，以及与所选 Display ID 配对的 Cursor 通道。验收内容
包括 alpha/color16/color32 光标更新、确认 client mouse mode 后的绝对移动、server mode
下的相对移动、按键和按钮边沿、motion ACK 恢复，以及四条传输的干净关闭。

TLS 测试需要启用 `oxide-spice-client/tls-ring`，并提供 rustls `ClientConfig`，由其中的
根证书或自定义校验器执行预期的 QEMU 身份验证。该功能会在传输边界编译 `ring` 的 C
与汇编代码，不会引入 OpenSSL 或 C SPICE 实现。

## 自动化测试覆盖

仓库测试覆盖 Main、两个 Display 通道、Inputs、Cursor 初始化前控制消息、显示器拓扑、
重置代次、16/24/32 位直接色解析、带填充的光标数据、原始索引调色板渲染、
`LZ_RGB32`、`LZ_PLT8`、`GLZ_RGB32`、`ZLIB_GLZ_RGB`、调色板失效、按钮前的指针顺序、
原始 Playback/Record、双向 Port 字节、usbredir Hello、通用数据包帧以及九任务关闭。

编解码测试覆盖 LZ 与 GLZ 行方向、分离 alpha、重叠和跨图像引用、扩展长度、错误引用、
输出上限与取消边界。模拟 peer 还覆盖 Main Agent token 协商、剪贴板交换、显示器布局、
文件成功/取消以及 Agent 重连重放。测试先在 Display 0 发送 GLZ 引用，再在 Display 1
发送其 zlib 包装的基础图像，以验证包装解码会发布到共享字典，等待过程也会释放解码
槽位。另一项测试在释放依赖流之前，让 Display 0 等待 Display 1 的 serial barrier。
