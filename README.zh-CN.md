# bo

> 给 agent 用的音频剪辑、混音与播放工具。一次一条命令——搭音轨、摆切片、调混音，然后实时播放或离线渲染成文件。现在还不是 DAW，但已经长着 DAW 的骨架。

[English](README.md) | [中文](README.zh-CN.md)

**bo** 是一个命令驱动的音频剪辑、混音与播放工具。你用 `put` 命令描述一个会话——把片段放到叠加的音轨上，每个片段是音频源的一个切片——用 `set` 调混音，再用 `play`（走声卡）或 `render`（离线混成 wav）听结果。没有项目文件：编排活在当前会话里——之后的命令继续操作同一个会话——可以写成命令脚本（`save`），也可以从脚本重建（`load`）。

每次 `bo ...` 调用就是一个动作：放一个片段、移除一个、移动播放头、调某条音轨的音量、开始或停止播放。daemon 按需自动拉起、用后自动清理，所以一次会话是一场对话，而不是一个项目。

## bo 是什么——以及不是什么

**bo** 是给 agent 用的剪辑、混音与播放工具：切片源、把片段摆上时间轴、叠音轨、调音量、淡入淡出与静音——编辑途中试听一段、把成品从头到尾完整放出，或渲染成文件。

它**现在还不是** DAW：没有效果器、没有 LFO/sidechain，也没有项目文件。clip 带增益、线性淡入淡出，以及——第一件自动化——能驱动 clip 声像或音量随播放移动的手画**曲线**、周期 **LFO**，或让音量被另一条总线电平压下去的 **sidechain**（`set clip.N.N.pan_control`、`set clip.N.N.gain_control`）；音轨带增益、静音和在立体声总线上的摆位（`set track.N.pan`）。摆位对立体声素材是 *balance*——衰减远端、保留宽度；对单声道素材是等功率 *pan*——能量分到两侧，声音在左右之间移动时既不变响也不变轻（中间也不再比原文件偏响 3 dB）。更宽的素材先下混到前两声道，所以 5.1 素材中置声道上的对白不会丢。若干音轨也可以路由进同一条**编组总线**（`bo route N 名字`）——一个汇流点，在主总线听到它们之前用一条属于自己的 strip 管它们加起来的声音，正是电台里用一个旋钮压住节目半边（音乐组/人声组）的那个东西。底层的数据模型——`Source` → `Clip` → `Track`、一条时间轴、一个 transport、音轨输出所喂的总线——正是 CLI DAW 赖以生长的骨架。路线图是在这副骨架上长出 DAW 的操作，而不是换一副骨架。

## 特性

- **编排即数据** —— 音轨和片段活在会话里，不在文件里。`save`/`load` 把它们序列化成当初构建它们的那一组命令。
- **为 agent 而生** —— 一次调用一个命令；回复是机器可读的文本；退出码稳定：2 = 用法错误，1 = 操作被拒绝。
- **实时播放，或离线渲染** —— 编辑途中可从播放头试听一段，成品可从头到尾完整放出（rodio），或把整个编排——乃至其中一段区间——离线混音成 wav。
- **播着改** —— 增益、淡化、声像或静音在设下的那一刻就落到正在跑的混音上；摆在某条音轨队尾之后的片段会直接加入正在跑的队列——所以节目可以边播边调、边播边续。`apply` 只留给正在跑的混音自己做不到的那类改动（take/move 一个已排队的片段），并且从声音真正所在的时码重建，而不是从墙上时钟。
- **编组总线** —— 用 `route` 把若干音轨并进一条总线，一条 strip（音量/静音）就能统一控制整组——电台的音乐组/人声组；`ls` 会显示各总线以及每条音轨喂给谁。路由与总线 strip 改动都是结构变更：和 take/move 一样落在下一次 `apply`。
- **源即手势** —— `set clip.N.N.pan_control '{"type":"curve","0":1,"3.2":-1}'` 把一条曲线插进 clip 的声像、`set clip.N.N.gain_control '{"type":"lfo","shape":"sine","rate":2,"depth":0.3}'` 把一个 LFO 插进它的音量、`set clip.N.N.gain_control '{"type":"sidechain","bus":"group.0","amount":-1.5}'` 让它听一条总线来闪避：听到的是静态基准加源随 clip 播放而变，现场播放与离线渲染完全一致（两边构建同一条链），改动像 fade 一样即时落在正在跑的混音上——不需要脚本去轮询播放头。今天有三种**控制源**——曲线、LFO、sidechain——骑在 clip 的声像与音量两个孔上。
- **静默回退** —— 没有音频设备时 daemon 照常工作；设 `BO_BACKEND=silent` 得到确定性的无头测试与 CI。
- **自动清理** —— 播放结束、`stop`、或非播放状态静默超过 `BO_IDLE_TIMEOUT` 秒（默认 600，`0` 禁用）时，daemon 退出并删除自己的 socket。
- **切片而非文件** —— `uri,from-to` 把源的任意切片放到时间轴的任意位置；in-point 在实时播放与离线渲染中都样本级精确；无需裁剪、无需拷贝。

## 安装

需要 **Rust 1.88 或更新版本**（edition 2024）。

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

一个双音轨会话——人声垫在音乐底上：

```console
$ bo put bed.wav,00:00:00-00:00:30          # 30 秒垫乐，放到新音轨上
ok: 1 clip on track 0
  clip #0 'bed.wav' 00:00:00.000-00:00:30.000 @ 00:00:00.000
$ bo put voice.wav,00:00:00-00:00:30 1@00:00:00      # 人声放到 1 号音轨
ok: 1 clip on track 1
  clip #0 'voice.wav' 00:00:00.000-00:00:30.000 @ 00:00:00.000
$ bo play
ok: 2 tracks, 2 clips, ends 00:00:30.000, playing from 00:00:00.000
$ bo set track.0.volume 0.4   # 把人声底下的垫乐压低，播放中直接生效
ok: `track.0.volume` set to `0.40`
$ bo set track.1.pan -0.6      # 把人声摆到中偏左，播放中直接生效
ok: `track.1.pan` set to `-0.60`
$ bo put outro.wav,00:00:00-00:00:10 0@00:00:30   # 播放中继续往队尾排
ok: 1 clip on track 0
  clip #1 'outro.wav' 00:00:00.000-00:00:10.000 @ 00:00:30.000
$ bo apply                    # 没有等着落地的改动
ok: nothing pending
$ bo stop                     # 结束会话；daemon 自动清理
ok: stopped
```

不带参数的 `bo set` 会把整个混音面打出来：一行状态行，之后每个当前值一行 `var value`——走的就是 `set` 自己应用的那张注册表——agent 一条命令就能读完（或快照）整个 session。

改动在被做出的那一刻就生效：增益和淡化直接进入正在播放它的那条链，摆在某条音轨队尾之后的片段直接加入正在跑的队列。`apply` 留给正在跑的混音自己做不到的那类改动——take/move 一个已排队的片段——它从声音真正所在的时码重建混音；确实得等的改动会多一行 `note:`，`ls` 把等待的数量报成 `pending=N`。

### 分组（编组总线）

若干音轨可以共用一条 strip——电台的音乐组/人声组——在主总线之前汇流：

```console
$ bo route 0 music                 # 音轨 0 的输出并入总线 'music'——首次提及即建、以它为名
ok: track 0 routed to bus #0 'music' (1 track)
$ bo route 1 music
ok: track 1 routed to bus #0 'music' (2 tracks)
$ bo set bus.0.volume 0.35         # 一个旋钮压住整条总线
ok: `bus.0.volume` set to `0.35`
$ bo ls
ok: 2 tracks, 2 clips
… 'silent' backend, … master=1.00, …
bus #0 'music' vol=0.35 tracks=2
  track 0 'bed' vol=1.00 pan=0.00 end=… bus=#0 'music'
  track 1 'voice' … bus=#0 'music'
```

总线 strip 在建图时烘焙进混音，所以改它（以及改路由）和 take/move 一个片段一样，落在下一次 `apply`。`route <track> master` 把音轨送回主总线；总线名唯一、是 `route` 的寻址对象（`master` 为保留字）；`set bus.N.muted true` 静音整组。总线、strip 与路由都随 `save`/`load` 往返；没有音轨喂的空总线不保存（空音轨本来也不保存）。

### 源取代脚本

演示里的"右扫左"曾经要一个后台脚本轮询播放头、每几十毫秒重摆一次位。变成源后只是一行，播放与渲染同源：

```console
$ bo put slide.wav,00:00:00-00:00:03.200
ok: 1 clip on track 0
$ bo set clip.0.0.pan_control '{"type":"curve","0":1,"3.2":-1}'  # 声像从 +1 走到 -1
ok: `clip.0.0.pan_control` set to `{"type":"curve","00:00:00.000":1,"00:00:03.200":-1}`
$ bo render mix.wav                        # 渲染出的文件和播放听到的是同一条扫
```

一个源 = 插进 clip 声像孔（`set clip.N.N.pan_control`）或音量孔（`set clip.N.N.gain_control`）的一根线。今天有三种，各自是一个单行 JSON 对象，`"type"` 标明是哪种：

- **曲线** —— 一张 时码→偏移 关键点表，点间线性、点外 hold：`{"type":"curve","0":1,"3.2":-1}`；
- **LFO** —— 周期摆动，字段 `shape`/`rate`/`depth`/`phase` 都可省略（默认 `sine`/`1`/`0.5`/`0`）：`{"type":"lfo","shape":"sine","rate":1,"depth":0.5}` 每秒半深摆一次；
- **sidechain** —— 监听一条总线（master，或你 route 出来的组）并跟随它的电平：`{"type":"sidechain","bus":"group.0","amount":-1.5}`——amount 是单位电平产生的偏移（负闪避、正上抬，默认 -0.5），attack/release 是时码（默认 5ms/150ms）。电台手势就是一行：把人声 route 进一个组，给音乐的 gain 接 `{"type":"sidechain","bus":"group.0"}`——音乐在人声下压下去、人声结束再弹回来。

时码可以按 CLI 的宽松写法输入——`3.2`、`0.005` 就是秒——回显统一成 `HH:MM:SS.fff`。

`none` 拔线。参数 = 静态基准 + Σ(活跃控制源)；每个输入孔今天各接一个源。

## 许可

BSD 3-Clause。见 [LICENSE](LICENSE)。
