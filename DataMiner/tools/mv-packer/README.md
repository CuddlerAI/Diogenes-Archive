# mv-packer — data_demo 转换产物 → mv-gameplay 数据集布局

> 音频扩展：旧 converter 输出继续按三成员、v1 manifest/index 原样打包。带 session audio 声明的
> 输出升级为 v2：仅 `ready` chunk 在原三成员后追加 `p0.flac`，`degraded/absent` 不造占位音频；
> `samples.jsonl/parquet`、session index 与 MANIFEST 会显式记录音频状态、格式和 SHA-256。
> 同一版本不允许混装旧 session 与音频 session，必须拆成独立 dataset config/version。
> `python -m unittest test_audio_contract.py` 覆盖成员顺序、降级状态、旧包隔离与汇总闭合。

把 `dataminer-data-demo convert` 的每 session 产物打成发布形态：按 session 分片的
tar、index 层（samples/framepairs parquet + 每 session 索引）、以及 `MANIFEST.json`。

## 为什么它在这里

v2026.07 是用一个**没有进仓库**的临时脚本打的。等到要发 v2026.08 时，那个脚本已经
不存在，格式只能从已发布产物逐字节反推（过程与结论见
[docs/52-mv-packaging-format.md](../../docs/52-mv-packaging-format.md)）。这次把它落进
仓库，就是为了不再发生第二次。

## 用法

```bash
python -m pip install -r requirements.txt
```

```bash
python pack_mv.py <converted-root> <out-root> <version>
```

`<converted-root>` 下每个 session 一个 `convert --output` 目录。产物写到
`<out-root>/<version>/`，可直接 `upload_folder` 到 HF。

## 与格式相关的几条硬约束

- **tar 手写而非用 `tarfile`**：参考产物的 `devmajor`/`devminor` 是全 NUL（不是八进制零），
  且结尾只有 1024 字节零（不补齐到 10240 字节记录）——Python 的 `tarfile` 两条都不满足。
- **`mtime` 恒为 0**、uid/gid 为 0、uname/gname 为空：打包结果因此只是内容的函数，
  与打包时间、打包机器无关，可复现。
- **分片上限 1 GiB，look-ahead 换 shard**（先判断放不放得下再决定开新 shard，不是超了再换）；
  chunk 三件套不跨 shard，shard 不跨 session。
- **`index/shards.json` 没有尾随换行**，是全库唯一的例外；差这一个字节 sha256 就对不上。
- JSON 一律 `indent=2` / 不排序键 / `ensure_ascii=False` / 带尾随换行；JSONL 一律紧凑分隔符，
  但**内嵌成字符串**的 `meta` / `actionT` 用的是 `json.dumps` 默认（带空格）分隔符。
