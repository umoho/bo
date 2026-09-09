# bo

面向自动化 agent 的音频编辑与混音引擎。`bo` 在常驻的 daemon 会话中维护一个编排（arrangement）——放置在堆叠轨道上的音频切片、汇入各 bus 的信号——并通过类型化的命令协议将其暴露给客户端。Rust 与 Python 客户端负责编排动词，仓库自带的命令行前端提供传输控制与快照恢复。

[English](README.md) | [中文](README.zh-CN.md)

## 架构

执行被集中在一处。引擎是 `Session::exec(Command)` 的唯一执行者；数据模型与命令词汇位于 `bo-core`。所有客户端经由同一 Unix socket 访问同一台 daemon，每个 socket 共享同一份 arrangement。

```
engine          Session：执行命令、驱动传输与后端
daemon (bin)    持有 Session；经 Unix socket 收发类型化 JSON；
                记录命令历史并执行快照操作
bo lib          bo::client::Bo：走 daemon wire 的类型化客户端
pybo            Bo 表面的 Python 绑定（pyo3，uv 管理）
mini CLI        bo play|pause|resume|seek|stop|load <快照>
```

引擎的公开面只有 `Session`。本仓库中 daemon 是引擎的唯一消费方；编排的修改不绕过命令路径。

## 接口

### 客户端（编排编辑）

编排动词由代码发出——控制流与计算决策属于代码：

- **Python** —— [`pybo`](py/)，基于 pyo3 的扩展，由 uv 构建。它绑定 `Bo` 表面及其值类型。时间接受秒或宽松文本，输出为 `HH:MM:SS.fff`；线上时长一律为整毫秒。
- **Rust** —— 本 crate 的 `bo::client::Bo`。

完整动词集为 `put`、`take`、`move`、`route`、`set`、`get`、`render`、`reset`、`apply`、传输动词，以及快照三件套 `save`/`load`/`check`。结构只能通过动词修改；arrangement 的读取与修改经由 `get`/`set`。

### 命令行前端

shell 接口仅保留终端擅长的操作：试听与恢复会话。

```console
$ bo play | pause | resume | seek <t> | stop | load <快照.bo>
```

退出码：`0` 成功，`1` 操作被拒，`2` 用法错误。

## 会话生命周期

daemon 由第一个访问某 socket 的客户端按需拉起（默认 `$TMPDIR/bo/daemon.sock`；可用 `--socket` 或 `socket=` 覆盖）。播放结束、收到 `stop`、或空闲超过 `BO_IDLE_TIMEOUT` 秒（默认 600，`0` 关闭）时，daemon 退出并移除自身 socket。相对源路径与渲染目标按发起进程的工作目录解析。

## 时间模型

所有表面使用同一时间方言。`Timecode` 接受秒（`1.23`）与宽松文本（`SS`、`MM:SS`、`HH:MM:SS`，可选 `.fff` 小数），规范输出为 `HH:MM:SS.fff`。回复与 arrangement 树中的时长均为整毫秒。

整源 `put` 会在会话处探测（其结尾通过解码确定）；闭区间的 `trim` 在播放或渲染之前不访问源文件。

## 数据模型

- **Source → Clip → Track。** clip 是源的一段 `from..to`，定位在轨道时间码 `at` 上。同轨 clip 互不重叠；轨道在混音中叠加。clip id 在同轨内稳定且不复用。
- **控制源。** 曲线、LFO 或 sidechain 可接入 clip 的 pan 或 gain 输入（`track.N.clips.M.pan_control`、`track.N.clips.M.gain_control`）。参数值等于静态基值加各源之和，在实时播放与离线渲染中一致求值。
- **Group bus。** `route` 将轨道输出导入组 bus；信号到达 master 之前由单一条（音量、静音）控制整组。
- **读取与写入 arrangement。** `get("")` 以 JSON 返回完整编排（轨道、clip、bus、传输状态）。`set(path, …)` 对状态区施加深 patch。结构成员关系不可 patch，只能通过动词修改。

## 编辑语义

状态编辑（增益、淡入淡出、pan、静音）在发出的当下作用于正在播放的混音。追加到轨道队列末尾之后的 clip 立即入队。运行中的图无法表达的编辑——取走或移动 clip、重新路由——被挂起，并在下一次 `apply` 时生效；`apply` 从当前实际播放位置重建图。

## 快照

快照即会话自身的命令历史加 playhead：一份带版本、可重放的脚本。`save` 写出；`load` 先在静默会话上 stage——坏快照不会改动当前会话——随后原子提交；`check` 只校验快照文件，不修改会话。

## 仓库结构

```
core/         数据模型与命令词汇
engine/       Session：唯一执行者；除此之外无公开面
bo lib        client::Bo over Connection（daemon wire）
src/cli.rs    迷你命令行前端（transport 与 load）
src/daemon.rs daemon：类型化 JSON wire；host 级快照
py/           pybo：Bo 表面的 pyo3 绑定（uv + maturin）
scripts/      install.sh / uninstall.sh
```

## 安装

前置要求：引擎、daemon 与 CLI 需要 **Rust 1.88+**；Python 绑定需要 **uv**。

```console
$ ./scripts/install.sh      # 安装 bo 二进制（cargo install）与 pybo
$ ./scripts/uninstall.sh    # 反向卸载两者
```

`bo` 安装至 `~/.cargo/bin`。`pybo` 构建为单个 abi3 wheel（Python ≥ 3.10），安装至 `$BO_PYTHON`、当前虚拟环境或 `python3`。若目标解释器为 externally-managed（PEP 668），设 `BO_PIP_BREAK=1`。`pybo` 拉起 daemon 需要 `bo` 在 `PATH` 上可达。

亦可在 checkout 内直接构建：

```console
$ cargo build --release
$ cd py && uv sync
$ uv run pytest
```

`BO_BACKEND=silent` 选择确定性无头后端，供测试与 CI 使用；无音频设备时 daemon 自动退回静音。

## License

BSD 3-Clause。见 [LICENSE](LICENSE)。
