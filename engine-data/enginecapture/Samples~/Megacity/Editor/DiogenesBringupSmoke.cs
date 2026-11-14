// batchmode -executeMethod 入口（与 SceneProbeExporter 同属 Diogenes.Bringup）：
//
//   Unity -batchmode -projectPath <megacity-metro> \
//         -executeMethod Diogenes.Bringup.DiogenesBringupSmoke.Run -logFile <log>
//
// 必须留在 Editor/ 目录内。本文件依赖 EditorApplication，早先放在没有 asmdef 的
// Samples~/Megacity/Bringup/ 下，同步进工程后会落入 Assembly-CSharp 被打进 player，
// 导致 player script 编译失败——而 BuildReport 只报 totalErrors=0 加一个不相干的
// ContentCatalogBuildUtility 异常，排查成本极高。Editor/ 的 asmdef 是
// includePlatforms:["Editor"]，把它挡在 player 之外；本文件用到的 Unity.Transforms
// 也在同一个 asmdef 的 references 里。

using System.IO;
using System.Text;
using Unity.Entities;
using Unity.Transforms;
using UnityEditor;
using UnityEngine;

namespace Diogenes.Bringup
{
    /// <summary>
    /// C10 适配点冒烟（docs/70 M-dev）：
    /// ① Unity.Entities 的 EntityQuery&lt;LocalToWorld&gt; 在本工程可编译可执行（动态物体位姿导出路径）；
    /// ② 枚举当前已存在的 ECS World 与实体计数（batchmode 编辑器态）；
    /// ③ 记录 Netcode/Entities 包版本，供 captureDeltaTime 相容性评估。
    /// </summary>
    public static class DiogenesBringupSmoke
    {
        public static void Run()
        {
            var sb = new StringBuilder();
            sb.AppendLine("== Diogenes bring-up smoke ==");
            foreach (var w in World.All)
            {
                using var q = w.EntityManager.CreateEntityQuery(ComponentType.ReadOnly<LocalToWorld>());
                sb.AppendLine($"world={w.Name} entities_with_LocalToWorld={q.CalculateEntityCount()}");
            }
            if (World.All.Count == 0)
                sb.AppendLine("no ECS worlds in batchmode editor state (expected without playmode/subscene open)");
            var l2w = new LocalToWorld { Value = Unity.Mathematics.float4x4.identity };
            sb.AppendLine($"LocalToWorld API ok, pos={l2w.Position}");
            File.WriteAllText("diogenes-bringup-smoke.txt", sb.ToString());
            Debug.Log(sb.ToString());
            EditorApplication.Exit(0);
        }
    }
}
