#!/bin/sh
# PreToolUse(Bash) hook:对正式桶 diogenes-depth-delivery 的任何**写**操作,
# 必须伴随 DataMiner/docs/105-r2-asset-manifest.md 的更新(工作区已改,或 HEAD 刚改过)。
# 台账即合同:桶里有而清单没有的数据,等于没入库(docs/105 §3)。
#
# 方向感知:transfer 类动词(copy/sync/move…)只拦「目的端=正式桶」——从桶下载不拦;
# 破坏性动词(purge/delete/mkdir/rmdir…)只要沾桶就拦。
# 退出码:0=放行;2=阻断并把 stderr 反馈给模型(Claude Code hooks 约定)。
# 紧急绕过:DIOGENES_R2_GUARD_SKIP=1(必须在随后的 PR/issue 里说明原因)。

payload=$(cat)

# 桶名根本没出现就立刻放行：本仓每次 Bash 调用都会过这个 hook，
# 让 99.9% 的调用免掉一次 python3 冷启动（~30–50ms），也不让 hook 依赖解释器存在。
case "$payload" in
  *diogenes-depth-delivery*) ;;
  *) exit 0 ;;
esac

verdict=$(printf '%s' "$payload" | python3 -c '
import json, re, shlex, sys

BUCKET = "diogenes-depth-delivery"
TRANSFER = {"copy", "copyto", "sync", "move", "moveto"}
DESTRUCTIVE = {"mkdir", "rmdir", "purge", "delete", "deletefile", "cleanup", "touch", "rcat"}
# 只读子命令。识别动词时**按已知子命令表匹配**，不能拿「第一个不以 - 开头的 token」当动词：
# rclone 允许全局旗标前置，`rclone --config x.conf copy 本地 远端:正式桶` 里那个
# `x.conf` 会被当成动词，于是 copy 认不出来、写操作直接放行。
READONLY = {
    "ls", "lsl", "lsd", "lsf", "lsjson", "size", "check", "cryptcheck", "cat",
    "md5sum", "sha1sum", "hashsum", "tree", "about", "version", "listremotes", "config",
}
KNOWN = TRANSFER | DESTRUCTIVE | READONLY

try:
    cmd = json.load(sys.stdin).get("tool_input", {}).get("command", "")
except Exception:
    # 载荷读不出来但桶名在场（shell 侧已确认）——从严
    print("block"); raise SystemExit
if BUCKET not in cmd:
    print("pass"); raise SystemExit

def is_write(segment: str) -> bool:
    if "rclone" not in segment:
        # 非 rclone 而沾桶名的段(如 aws s3 rm)从严:出现写动词即拦
        return bool(re.search(r"\b(rm|cp|mv|sync|put|delete)\b", segment)) and BUCKET in segment
    try:
        toks = shlex.split(segment)
    except ValueError:
        return True  # 解析不了的沾桶命令,从严拦下让人看一眼
    try:
        i = toks.index("rclone")
    except ValueError:
        return False
    rest = toks[i + 1:]
    verb = next((t for t in rest if t in KNOWN), "")
    if not verb:
        # 认不出子命令又沾着桶名：可能是新版动词或被变量拼出来的，从严拦。
        return True
    positionals = [t for t in rest[rest.index(verb) + 1:] if not t.startswith("-")]
    if verb in READONLY:
        return False
    if verb in DESTRUCTIVE:
        return any(BUCKET in p for p in positionals)
    if verb in TRANSFER:
        return len(positionals) >= 2 and BUCKET in positionals[-1]
    return False

for seg in re.split(r"[|;&]+", cmd):
    if BUCKET in seg and is_write(seg):
        print("block"); raise SystemExit
print("pass")
' 2>/dev/null)

# python3 缺失或脚本本身炸了 → verdict 为空。桶名在场却判不出方向时按阻断处理：
# 这个 hook 的存在意义就是「未登记不得写正式桶」，判不出来时放行等于没装。
[ -n "$verdict" ] || verdict="block"

[ "$verdict" = "block" ] || exit 0

if [ "$DIOGENES_R2_GUARD_SKIP" = "1" ]; then
  echo "[r2-guard] 已按 DIOGENES_R2_GUARD_SKIP=1 放行——请在 PR/issue 里说明本次绕过。" >&2
  exit 0
fi

manifest="DataMiner/docs/105-r2-asset-manifest.md"
root="${CLAUDE_PROJECT_DIR:-.}"

# 满足其一即放行:工作区里 manifest 有未提交修改;或 HEAD 提交触碰了它(先提交后上传的流程)。
if git -C "$root" status --porcelain -- "$manifest" 2>/dev/null | grep -q .; then
  exit 0
fi
if git -C "$root" log -1 --name-only --format= 2>/dev/null | grep -qx "$manifest"; then
  exit 0
fi

cat >&2 <<EOF
[r2-guard] 拦截:检测到对正式桶 diogenes-depth-delivery 的写操作,
但 $manifest 没有同步更新(工作区无修改,HEAD 也未触碰)。
正式数据入桶的台账规则(docs/105 §3):先在 manifest 登记本次入库条目,再执行上传。
紧急绕过:DIOGENES_R2_GUARD_SKIP=1,并在 PR/issue 里说明原因。
EOF
exit 2
