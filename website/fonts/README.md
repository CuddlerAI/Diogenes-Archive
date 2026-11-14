# fonts — 自托管字体

设计文档 §3.3 指定的三款字体，自托管而非引第三方 CDN（隐私 + 稳定性）。
`@font-face` 规则在 [`../styles.css`](../styles.css) 顶部，改字体只需改那里。

## 许可

三款**全部是 SIL Open Font License 1.1**，自托管无需购买授权。OFL 要求随字体分发许可证，
因此 `OFL-*.txt` 与 woff2 放在一起，**不要删**。

| 字体 | 角色 | 上游 |
|---|---|---|
| Space Grotesk | `--display`（大标题 / 模型名 / 字标） | [floriankarsten/space-grotesk](https://github.com/floriankarsten/space-grotesk) |
| Inter | `--sans`（正文 / 按钮） | [rsms/inter](https://github.com/rsms/inter) |
| IBM Plex Mono | `--mono`（`//` kicker / 终端块 / meta） | [IBM/plex](https://github.com/IBM/plex) |

## 文件构成

10 个 woff2，仓库里共 ~226 KB。Space Grotesk 与 Inter 是**可变字体**（一个文件覆盖整个字重范围，
比拆成 5 个静态档更省）；IBM Plex Mono 无可变版本，取 500 / 600 / 700 三档（站上实际用到的）。

**只发 latin + latin-ext。** 每条 `@font-face` 都带 `unicode-range`，浏览器**只下载页面真正
出现的 subset**——英文页面实际拉约 99 KB，不是仓库里那 226 KB。中文按 §3.3 回退到
PingFang SC / Microsoft YaHei，**不发 CJK webfont**（那会是几 MB 量级）。

### 已知回退（无害，自托管之前就是这样）

演示块里的 `Δ`、`→` 以及 `◎ ◇ ⟡ ▶ ■ ↺` 这一组几何符号，不在任何已发布的 latin range 内
（Google 对 IBM Plex Mono 根本没有提供 greek subset）。这些字形由 mono 栈里的下一个字体渲染。
显示正常，只是不是 Plex 的字形。

## 缓存约束 ⚠️

`_headers` 给 `/fonts/*` 设了 30 天缓存。文件名编码的是 **subset + 字重，不是内容哈希**——
所以**换字体必须同时改文件名**，否则已缓存的访客最长 30 天拿不到新字体。

## 重新生成

从 Google Fonts 取的。要更新版本或增减字重，按 `../styles.css` 里现有规则的模式操作：

1. 用现代浏览器 UA 请求 `https://fonts.googleapis.com/css2?family=<Family>:wght@<...>&display=swap`
   （UA 决定返回 woff2 还是 ttf）
2. 从返回的 CSS 里按 `/* subset */` 注释挑出需要的 `@font-face`，下载其 `url()`
3. 把 `src` 改成 `/fonts/<本地文件名>`，**保留原样的 `unicode-range`**——它是分片下载生效的关键
4. 同步更新 [`../styles.css`](../styles.css) 顶部的 preload（只 preload 首屏用到的两个 latin 档）
