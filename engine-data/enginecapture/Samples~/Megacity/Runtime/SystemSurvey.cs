// docs/78 G'6 / 辩题十一 🟨：采集世界系统勘察表。把采集世界里实际存在的系统逐组导出，
// 与 client 世界应有集合的差集（被过滤掉的 ClientSimulation-only 系统）一并列出，
// 归档进 BRINGUP-MEGACITY.md 的勘察表。

using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text;
using Unity.Collections;
using Unity.Entities;

namespace Diogenes.Capture.Megacity
{
    public static class SystemSurvey
    {
        /// <summary>Write a markdown survey of (a) systems present in the capture world
        /// (every ComponentSystemGroup with its direct members, unmanaged ISystem included
        /// via SystemState.DebugName), (b) systems that would exist in a netcode client
        /// world but were filtered out of ours.</summary>
        public static void WriteReport(World world, string path)
        {
            var sb = new StringBuilder(1 << 16);
            sb.Append("# 采集世界系统勘察表（自动生成）\n\n");
            sb.Append("世界: ").Append(world.Name).Append("  \n");
            sb.Append("过滤: Default | LocalSimulation | Streaming\n\n");

            sb.Append("## 采集世界内实际存在的系统（按组）\n\n");
            int total = 0;
            foreach (var s in world.Systems)
            {
                if (!(s is ComponentSystemGroup group)) continue;
                sb.Append("### ").Append(group.GetType().Name).Append('\n');
                using (var handles = group.GetAllSystems(Allocator.Temp))
                {
                    for (int i = 0; i < handles.Length; i++)
                    {
                        string name = DebugName(world, handles[i]);
                        sb.Append("- ").Append(name).Append('\n');
                        total++;
                    }
                }
                sb.Append('\n');
            }
            sb.Append("组内成员合计 ").Append(total).Append(" 条（子组成员在其子组标题下重复列出）。\n\n");

            sb.Append("## 被过滤掉的 client 侧系统（ClientSimulation 命中但本世界无）\n\n");
            var captureSet = new HashSet<string>(
                DefaultWorldInitialization.GetAllSystems(
                        WorldSystemFilterFlags.Default
                        | WorldSystemFilterFlags.LocalSimulation
                        | WorldSystemFilterFlags.Streaming)
                    .Select(t => t.FullName));
            var clientOnly = DefaultWorldInitialization
                .GetAllSystems(WorldSystemFilterFlags.ClientSimulation)
                .Select(t => t.FullName)
                .Where(n => !captureSet.Contains(n))
                .OrderBy(n => n)
                .ToList();
            foreach (var n in clientOnly)
                sb.Append("- ").Append(n).Append('\n');
            sb.Append("\n共 ").Append(clientOnly.Count).Append(" 个 client-only 系统被剥离。\n");

            Directory.CreateDirectory(Path.GetDirectoryName(path));
            File.WriteAllText(path, sb.ToString());
        }

        static string DebugName(World world, SystemHandle handle)
        {
            try
            {
                ref var state = ref world.Unmanaged.ResolveSystemStateRef(handle);
                return state.DebugName.ToString();
            }
            catch (System.Exception)
            {
                return "<unresolvable system handle>";
            }
        }
    }
}
