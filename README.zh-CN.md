# bo

> 一个"编排即数据"的播放器。一次一条命令，编排并播放广播节目——从 shell 里，或由 agent 驱动。

[English](README.md) | [中文](README.zh-CN.md)

**bo** 是一个命令驱动的广播播放器。你用 `put` 命令描述一个节目——把片段放到叠加的音轨上，每个片段是音频源的一个切片——然后 `play` 播放。没有项目文件：编排只活在常驻 daemon 的内存里，通过 Unix socket 访问，可以写成命令脚本（`save`），也可以从脚本重建（`load`）。

每次 `bo ...` 调用就是一个动作：放一个片段、移动播放头、压低某条音轨、开始或停止播放。daemon 按需自动拉起、用后自动清理，所以一次会话是一场对话，而不是一个项目。

## 特性

- **编排即数据** —— 音轨和片段活在 daemon 内存里，不在文件里。`save`/`load` 把它们序列化成当初构建它们的那一组命令。
- **为 agent 而生** —— 一次调用一个命令；回复是机器可读的文本；退出码稳定：2 = 用法错误，1 = 操作被拒绝。
- **实时播放，或离线渲染** —— 通过声卡播放（rodio），或把整个节目——乃至其中一段区间——离线混音成 wav。
- **静默回退** —— 没有音频设备时 daemon 照常工作；设 `BO_BACKEND=silent` 得到确定性的无头测试与 CI。
- **自动清理** —— 节目播完、`stop`、或非播放状态静默超过 `BO_IDLE_TIMEOUT` 秒（默认 600，`0` 禁用）时，daemon 退出并删除自己的 socket。
- **切片而非文件** —— `uri@at:from-to` 把源的任意切片放到时间轴的任意位置，无需裁剪、无需拷贝。

## 安装

需要 **Rust 1.85 或更新版本**（edition 2024）。

```console
$ cargo build --release
$ target/release/bo --help
```

或从本目录安装：

```console
$ cargo install --path .
```

crates.io 上的 `bo` 名字已被占用，`cargo install bo` 装的是另一个无关的 crate。请从源码构建，或使用发布版的二进制。

## 快速上手

```console
$ bo put bed.wav:00:00:00-00:00:30          # 30 秒垫乐，放到新音轨上
ok: track 0 clip #0 bed.wav @ 00:00:00.000
$ bo put voice.wav:00:00:00-00:00:30 1      # 人声放到 1 号音轨
ok: track 1 clip #0 voice.wav @ 00:00:00.000
$ bo play
session: 2 tracks | 2 clips | ends 00:00:30.000 | backend rodio
playing from 00:00:00.000
$ bo set track.0.volume 0.4   # 把人声底下的垫乐压低
track 0 volume 0.40
$ bo apply                    # 让改动立即生效
apply: rebuilt from 00:00:00.000
$ bo stop                     # 结束会话；daemon 自动清理
stopped
```

## 许可

BSD 3-Clause。见 [LICENSE](LICENSE)。
