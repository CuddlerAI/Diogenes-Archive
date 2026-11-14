import { availableParallelism } from 'node:os'
import { defineConfig } from 'vitest/config'

// 这批测试里有二十多个文件各起一份 PGlite（WASM 版 Postgres），等待远多于计算。
// 托管 ubuntu runner 实测只有 **2 核**（nproc=2 / availableParallelism=2 / 8GB），
// 而 vitest 默认按可用并行度扣掉主进程，实际只剩 1 个 worker —— 于是墙钟约等于
// 所有测试时长之和，文件级并行等于没有。
//
// 同一个包在 CI 上的三档实测（run 32937981980）：
//   默认（≈1 worker）        墙钟 137.7s / 测试和 128.1s → 1.07x
//   forks   maxWorkers=4     墙钟  91.9s / 测试和 328.0s → 3.6x
//   threads maxWorkers=4     墙钟  66.9s / 测试和 233.0s → 3.5x   ← 采用这档
//
// 两个反直觉但实测成立的点：
//   1. 2 核机器上开 4 个 worker 反而快一倍 —— 因为瓶颈是等待不是算力，超额订阅填满了空闲。
//   2. threads 比 forks 再快 27% —— WASM 堆不用随进程复制。
//
// ⚠ 必须用顶层 `maxWorkers`，不是 `poolOptions.threads.maxThreads`：
//   后者在 vitest 4.x 上被静默忽略。本机对照实测（把值调到 1 看会不会变慢）：
//     poolOptions.threads.maxThreads = 1  →  墙钟 16s（仍并行，没生效）
//     maxWorkers = 1                      →  墙钟 46s（生效）
//
// 取 max(4, 本机核数)：CI 上是 4，开发机（多核）不被压到比默认还低。
const workers = Math.max(4, availableParallelism())

export default defineConfig({
  test: {
    pool: 'threads',
    maxWorkers: workers,
    minWorkers: Math.min(2, workers),
  },
})
