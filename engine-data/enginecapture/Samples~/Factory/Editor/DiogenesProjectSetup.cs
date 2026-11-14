// 一次性工程初始化：创建最小 URP 管线资产并设为默认（冒烟用；
// Factory Training 导入后若自带 URP 设置，以其为准再切换）。
using UnityEditor;
using UnityEngine;
using UnityEngine.Rendering;
using UnityEngine.Rendering.Universal;

public static class DiogenesProjectSetup
{
    public static void CreateUrp()
    {
        if (!AssetDatabase.IsValidFolder("Assets/Settings"))
            AssetDatabase.CreateFolder("Assets", "Settings");

        var rendererData = ScriptableObject.CreateInstance<UniversalRendererData>();
        AssetDatabase.CreateAsset(rendererData, "Assets/Settings/DiogenesRenderer.asset");

        var rp = UniversalRenderPipelineAsset.Create(rendererData);
        AssetDatabase.CreateAsset(rp, "Assets/Settings/DiogenesURP.asset");

        GraphicsSettings.defaultRenderPipeline = rp;
        QualitySettings.renderPipeline = rp;
        AssetDatabase.SaveAssets();
        Debug.Log("[setup] URP asset created and assigned");
        EditorApplication.Exit(0);
    }
}
