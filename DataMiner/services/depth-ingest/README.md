# depth-ingest（独立实验服务）

`depth-ingest` 把 `tools/depth-lab/src/archive_contract/ingest.rs` 的 #535 状态机接成
独立 HTTP 服务。它只服务 `dataminer-depth-lab`，不注册到 `apps/api`，不读取生产
`R2_BUCKET`，不写 `artifact_objects`，也不接受 production uploader credential。

## 边界

- 客户端先持有 audience=`dataminer-depth-ingest` 的短期 experiment JWT；服务不签发
  token，只验证独立 HMAC key。
- R2 access key/secret 仅由本服务进程持有。客户端只收到短期 presigned URL，以及必须
  原样发送的 `content-length`、`x-amz-content-sha256` 等签名头。
- S3 multipart URL 必然带 provider `uploadId` query；它是短期 bearer URL 的 opaque
  部分，不作为 API 字段返回，不得写客户端 durable queue 或日志。
- complete 后服务端完整 GET/流式回读并计算 SHA-256。multipart ETag 只交给 R2
  complete，绝不充当内容 SHA-256。
- payload 全部通过 size/SHA 后才签 manifest PUT；manifest 授权固定签入
  `If-None-Match: *`，随后再次下载、计算 SHA-256、解析并逐字段比对才 finalized。
- durable index 保存 #535 快照与 provider upload id，但不保存任何 R2/auth secret。
  快照损坏、CAS 冲突或落盘失败会关闭 readiness 并 fail-closed。

## HTTP 契约

除健康检查外均使用 JSON。archive 请求继续使用 #535 的完整 `ArchiveScope`；路径里的
archive/upload id 必须与 body 一致。

| Method | Path | Audience | 作用 |
|---|---|---|---|
| `POST` | `/depth-ingest/v1/archives` | `dataminer-depth-ingest` | 注册 manifest/scope |
| `POST` | `/depth-ingest/v1/archives/{archive}/multipart` | 同上 | 创建对象 multipart |
| `POST` | `.../multipart/{upload}/parts/authorize` | 同上 | 签一个精确 part grant |
| `POST` | `.../multipart/{upload}/complete` | 同上 | complete + 全对象回读 SHA |
| `POST` | `.../manifest/authorize` | 同上 | payload 完整后签 conditional PUT |
| `POST` | `.../manifest/finalize` | 同上 | 回读 manifest 并 finalized |
| `POST` | `.../status` | 同上 | 查询完整 scope 状态 |
| `POST` | `/depth-ingest/internal/v1/cleanup/orphan-multipart` | `dataminer-depth-ingest-worker` | 清理过期且未 complete 的 multipart |
| `GET` | `/healthz` / `/readyz` | 无 | 进程/持久化状态 |

错误只返回稳定 code，不回显 provider 响应体、presigned URL 或 secret。

## 独立配置

服务只读取以下 `DEPTH_INGEST_*` 名称：

- `DEPTH_INGEST_BIND_ADDR`（默认 `127.0.0.1:8080`）
- `DEPTH_INGEST_AUTH_HMAC_SECRET`（至少 32 bytes，独立 secret）
- `DEPTH_INGEST_INDEX_DIR`（单实例持久卷目录）
- `DEPTH_INGEST_GRANT_TTL_SECONDS`（60–900，默认 300）
- `DEPTH_INGEST_ORPHAN_GRACE_SECONDS`（默认 1200）
- `DEPTH_INGEST_R2_ACCOUNT_ID`
- `DEPTH_INGEST_R2_BUCKET`（必须等于 `dataminer-depth-lab`）
- `DEPTH_INGEST_R2_ACCESS_KEY_ID`
- `DEPTH_INGEST_R2_SECRET_ACCESS_KEY`
- `DEPTH_INGEST_R2_REQUEST_TIMEOUT_SECONDS`（30–300，默认 120）
- `DEPTH_INGEST_MAX_MANIFEST_BYTES`（默认 16 MiB，上限 64 MiB）

`R2_BUCKET`、生产 uploader token 等名称即使存在也不会被读取。

## 本地验证

```powershell
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --locked --release
```

crate 自带独立 `Cargo.lock`，不会改 DataMiner workspace/Cargo.lock。Docker build context 是
`DataMiner/`：

```powershell
docker build -f services/depth-ingest/Dockerfile -t dataminer-depth-ingest .
```

## 部署前清单（本 PR 不执行）

1. 建独立 `dataminer-depth-lab` bucket、180 天对象 lifecycle 与未完成 multipart cleanup。
2. 建只对该桶 Object Read & Write 的 R2 token；通过 Secret Manager 注入上述独立变量。
3. 现网摄取 token → depth bucket 必须 403；depth token → 现网摄取桶和第三桶必须 403。
4. experiment JWT issuer 使用独立 HMAC key/audience；production uploader token 对本服务应 403。
5. 当前文件 repository 只准单实例 + 持久卷。Cloud Run 多实例部署前，实现同一
   `DepthIndexRepository` 的托管数据库 CAS，并完成 R2 side-effect/DB crash fault injection。
6. 用真实 R2 验收 multipart presign 的 `content-length` / `x-amz-content-sha256` 签名头、
   complete 404 重放恢复、全对象回读 SHA、conditional manifest 412 与 cleanup。
7. 日志/网关禁记 Authorization、presigned query、provider upload id 和 secret。

U2 客户端 helper 只消费独立 depth queue、调用上述 HTTP、执行 presigned PUT，并保存
逻辑 archive/upload id 与本地 hash；它不接触 R2 长期凭据，也不调用 production uploader。
