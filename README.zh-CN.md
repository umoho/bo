# bo

> 给 agent 用的音频编辑、混音与播放器。建轨道、放素材、调混音，然后实时试听或离线渲染——用 Python 脚本、Rust 程序或 shell 都能驱动。还不是 DAW，但这是会长成 DAW 的骨架。

[English](README.md) | [中文](README.zh-CN.md)

**bo** 是一个命令驱动的音频引擎。你描述一个会话——把音频源的切片放到叠层轨道上、调好混音——然后用 `play`（出声设备）或 `render`（离线混成 wav）听结果。没有工程文件：arrangement 活在 daemon 会话里，可以写成**快照**（`save`）再重建（`load`）。

## 一张脸是代码，一张脸是 shell，共用一个会话

数据模型与引擎是主干，之上的每一层都通过同一条 typed 命令 wire 与同一台 daemon 对话，因此每一条 socket 上各方共享同一个 arrangement。

* **代码才是编辑器。** 编排动词——放、取、移动、路由、patch、渲染——都在 typed 客户端里：
  * **Python**：[`pybo`](py/)（pyo3 扩展，uv 管理），见下面的[快速开始](#快速开始python)。
  * **Rust**：本 crate 的 `bo::client::Bo`。
* **shell 是 transport。** 迷你 CLI 只留终端真正擅长的：试听与恢复：
  ```console
  $ bo play | pause | resume | seek <t> | stop | load <快照.bo>
  ```

daemon 由第一个碰 socket 的客户端按需拉起（默认 `$TMPDIR/bo/daemon.sock`；可用 `--socket`/`socket=` 指定别的），并在播放结束、`stop` 或静默超过 `BO_IDLE_TIMEOUT` 秒后自清理（默认 600，`0` 关闭）。

## 快速开始（Python）

```python
import pybo

bo = pybo.Bo()                                  # 共享 daemon
bo.put(pybo.trim("voice.wav", "0:30-1:00"),     # 源的一段切片
       pybo.Track(0).at("0:00"))                # 放到轨道 0 起点
bo.put("bed.wav", pybo.Track(1).at("0:00"))     # 整源（按需探测长度）
bo.route(on=1, bus="music")                     # 多轨进同一 bus
bo.route(on=0, bus="music")
bo.set("bus.0.volume", 0.35)                    # 一个旋钮压整条 bus
bo.set("track.0.volume", 0.4)
bo.get("track.1")                               # 把树读回来
bo.render("mix.wav")
bo.save("show.bo")                              # 存一份快照
```

时间只有一种说法：`Timecode(1.23)` 与 `Timecode("1.23")` 都是 1.23 秒，
`Timecode("1:02.5")` 是一分多，`str(t)` 输出 `HH:MM:SS.fff`。回复与树里的时长都是整毫秒。整源 `put` 会在 arrangement 所在处探测（确定结尾）；闭区间 `trim(...)` 在播放/渲染前不碰盘。错误是 typed 的（`pybo.BoError`）。

用 shell 恢复并试听：

```console
$ bo load show.bo
ok: loaded 'show.bo'
$ bo play
ok: 2 tracks, 2 clips, ends 00:01:00.000, playing from 00:00:00.000
$ bo stop
ok: stopped
```

退出码：`0` 成功，`1` 被拒，`2` 用法错。曾经是文本命令的编排动词——`put`、`ls`、`set`、`render`、`save`……——现在都是客户端调用；`bo help` 说明 shell 面。

## 数据模型

* **Source → Clip → Track**：clip 是源的一段 `from..to`，停在轨道的某个时间码（`at`）；同一条轨道内部不重叠，跨轨道叠成混音。每条轨道内的 id 稳定、不复用。
* **源即手势** —— 曲线、LFO、sidechain 可插进 clip 的 pan 或 gain 输入（`track.N.clips.M.pan_control` / `gain_control`）：`{"type":"curve","0":1,"3.2":-1}`、`{"type":"lfo","shape":"sine","rate":1,"depth":0.5}`、`{"type":"sidechain","bus":"group.0"}`——参数在静态基值上叠加该源，现场与离线渲染一致。
* **Group bus**：`route` 把若干轨收进一个 bus；进 master 之前，一个条（音量、静音）控整组——电台音乐 bus 或人声 bus。
* **树即读写面**：`get("")` 把整个 arrangement（轨道、素材、bus、transport）作为 JSON 返回；`set("track.0", …)` 对状态区做深 patch。结构只能由动词改。

## 播放中编辑

增益、淡入淡出、pan、静音在设置当下就落到正在播放的混音上；放到轨道队列末尾之后的素材直接入队。运行中的图接不住的改动——取走/移动素材、重新路由——会挂起（`landed: pending`），等一次 `apply` 从真实听到的位置重建。

## 快照

快照（`save`）就是会话自己的命令历史加 playhead：一份带版本、可重放的脚本。`load` 原子地 staging——坏快照不会碰当前会话；`check` 只校验快照文件、不动会话。

## 目录结构

```
core/     模型单元：Command/Outcome、树、时间码文本
engine/   唯一执行者：Session::exec（transport/backend 之上）
bo lib    client::Bo over Connection（daemon wire）；0.2 内冻结
src/cli.rs  迷你 CLI（transport + load）
src/daemon.rs  daemon：JSON wire 进、JSON wire 出；host 级快照
py/        pybo：Bo 面的 pyo3 绑定（uv + maturin）
```

## 安装

引擎、CLI、daemon 需要 **Rust 1.88+**；Python 绑定需要 **uv**。

```console
$ cargo build --release          # bo CLI/daemon
$ cd py && uv sync               # pybo 装进 .venv
$ uv run pytest                  # pybo 测试
```

Python 宿主自己找 daemon 二进制（本 checkout 的 `target/…/bo`，其次 `PATH`），脚本只要 import 即可。

需要确定性无头会话与 CI 时设 `BO_BACKEND=silent`；没有音频设备时 daemon 自动退回静音并给出提示。

## License

BSD 3-Clause. 见 [LICENSE](LICENSE)。
