# pybo 用法

pybo 0.2.0 是 bo 的 Python 客户端（pyo3/uv，abi3，Python ≥ 3.10）。它只经
daemon 驱动编排；所有动词都在引擎侧执行。拿不准某页时：
`python3 -c "import pybo; print(pybo.help('<话题>'))"`（`pybo.help()` 列出全部）。

## 连接

```python
import pybo
bo = pybo.Bo()                      # 共享 daemon：$TMPDIR/bo/daemon.sock
bo = pybo.Bo(socket="/tmp/x.sock")  # 独立会话（socket 路径 ≈ ≤104 字节）
# 同一 socket 的多个 Bo / 进程 / `bo` CLI 共享同一 arrangement
# daemon 按需拉起：$BO_DAEMON → PATH 上的 bo → 本 checkout 的 target/…/bo
```

## 时间（一种方言）

```python
pybo.Timecode(1.23).ms        # 1230      秒 / 文本入 → HH:MM:SS.fff 出
str(pybo.Timecode("1:02.5"))  # '00:01:02.500'
pybo.Timecode.from_ms(1230)   # 树/回复里的 ms → 可打印 Timecode
# 任意收"时刻"的参数可直接给秒 float 或文本：
pybo.Track(0).at("0:30"); bo.seek("0:05")
```

## 素材与落点

```python
pybo.trim("v.wav")                      # 整源（开尾 → 放置时探测，文件须存在）
pybo.trim("v.wav", "0:30-1:00")         # 闭区间（不碰盘，直到播放/渲染）
pybo.trim("v.wav", "0:30-")             # 到源尾
pybo.trim("v.wav", start="0:30", end=90)  # kw 形式：start/end
m = pybo.trim("v.wav", "0:30-1:00"); m.uri; m.from_ms; m.to_ms

pybo.Track(0)                # 轨道 0 @ playhead
pybo.Track(2).at("1:00")     # 轨道 2 @ 1:00
pybo.Track.fresh()           # 新轨道 @ playhead
pybo.Track.fresh(at="0:05")  # 新轨道 @ 0:05
```

## Bo 动词

回复都是 dict；其中时长一律是**整毫秒**；`landed` = `"live"`（运行中的混音已接）
或 `"pending"`（等下一次 `apply`/`play`）。

```python
bo.put(material, dest=None)   # → {track, clip:{id,uri,at,from,to,gain}, landed}
                              #   material: uri 字符串（整源）| trim(...) 切片
                              #   dest 省略 = 新轨道 @ playhead
bo.take(on=0, clip_id=3)      # → {track, clip, landed}；clip_id 或 at= 覆盖时刻，二选一
bo.move(on=0, clip_id=3, to=pybo.Track(1).at("0:00"))  # to 必填
bo.route(on=1, bus="music")   # → {track, bus, landed}；bus: "master" | int id | 名字
bo.set(path, value)           # → {path, patched, landed}
bo.get(path="")               # → 树 dict（见下）
bo.render(file, trim=None, measure=False, mono=False)  # → {file, duration_ms, stats}
bo.play()                     # → {tracks, clips, end, playhead}
bo.pause() / bo.resume()      # → {at}
bo.seek("0:05") / bo.stop() / bo.reset()              # → None
bo.apply()                    # → "nothing pending" | {"kind": …}
bo.save("show.bo")            # → {version, playhead, commands}
bo.load("show.bo") / bo.check("show.bo")              # → None
```

## 树与 set

```python
tree = bo.get("")
# { master: {volume},
#   transport: {state, playhead, duration},
#   track: [{name, volume, pan, muted, clips:[{id,uri,from,to,at,gain,
#            fade_in,fade_out,pan,pan_control,gain_control}]}],
#   bus:  [{name, volume, muted, members}] }

bo.set("master.volume", 0.9)
bo.set("track.0", {"muted": True, "name": "bed"})   # 对象 = merge（缺省键不动）
bo.set("track.0.clips.0.gain", 0.7)                 # M = 轨内下标（不是 id）
bo.set("track.0.clips.0.pan_control", None)         # None 拔线
# 结构（track[] / bus[] / clips 归属）只能走动词；set 里出现 clips 键会被拒
# fade_in/fade_out 在 set 里是 ms 整数（500），不是文本
```

## 最小一例

```python
import pybo
bo = pybo.Bo()
bo.put(pybo.trim("voice.wav", "0:30-1:00"), pybo.Track(0).at("0:00"))
bo.put("bed.wav", pybo.Track(1).at("0:00"))
bo.route(on=1, bus="music"); bo.route(on=0, bus="music")
bo.set("bus.0.volume", 0.35)
bo.render("mix.wav")
bo.save("show.bo")
bo.stop()
```

## 与 CLI 交接

CLI 只有六个动词（无编排动词）：

```bash
bo load show.bo   # 恢复 pybo save 的快照
bo play | pause | resume | seek <t> | stop
```

## 错误

- `pybo.BoError`：引擎/daemon 拒绝（重叠、文件不可测、无此 clip/bus、版本不符…）
- `ValueError`：参数值错（坏时间码、`end` 早于 `start`、range 与 start/end 同时给…）
- `TypeError`：类型错（`bo.put(x, 0)`）

## 检查/清理

```bash
cd py && uv sync            # 装依赖 + pybo 到 .venv
uv run pytest               # 测试
```
