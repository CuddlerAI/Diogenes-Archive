// 用编辑器自身的 Unity 会话在官方 Package Manager 端点上：
//   1) 查 344832 的购买态；2) 未持有则 acquire（免费资产）；3) 取下载信息；
//   4) 下载 .unitypackage 到本地；5) 导入工程。
// token 只在本进程内使用（CloudProjectSettings.accessToken），不打印、不外传。
// 产物日志 /tmp/diogenes-store-acquire.txt（只记状态码与长度）。
using System;
using System.Collections;
using System.IO;
using System.Text;
using UnityEditor;
using UnityEngine;
using UnityEngine.Networking;

public static class DiogenesStoreAcquire
{
    const string ProductId = "344832";
    const string Api = "https://packages-v2.unity.com/-/api";
    static readonly string PkgPath = "/Users/example/Documents/factory-training.unitypackage";
    static readonly StringBuilder Log = new StringBuilder();

    [MenuItem("Diogenes/Acquire Factory Training")]
    public static void Run()
    {
        EditorCoroutine.Start(Flow());
    }

    static void W(string s)
    {
        Log.AppendLine(s);
        Debug.Log("[acquire] " + s);
        File.WriteAllText("/tmp/diogenes-store-acquire.txt", Log.ToString());
    }

    static UnityWebRequest Req(string url, string method, string body)
    {
        var req = new UnityWebRequest(url, method);
        if (body != null)
            req.uploadHandler = new UploadHandlerRaw(Encoding.UTF8.GetBytes(body));
        req.downloadHandler = new DownloadHandlerBuffer();
        req.SetRequestHeader("Authorization", "Bearer " + CloudProjectSettings.accessToken);
        req.SetRequestHeader("Content-Type", "application/json");
        return req;
    }

    static IEnumerator Flow()
    {
        // 会话 token 在编辑器启动后才由 Hub/Connect 注入，先等（最多 120s）
        double t0 = EditorApplication.timeSinceStartup;
        string tok = CloudProjectSettings.accessToken;
        while (string.IsNullOrEmpty(tok) && EditorApplication.timeSinceStartup - t0 < 120)
        {
            yield return null;
            tok = CloudProjectSettings.accessToken;
        }
        W("tokenLen=" + (tok == null ? 0 : tok.Length)
            + " waited=" + (EditorApplication.timeSinceStartup - t0).ToString("F0") + "s");
        if (string.IsNullOrEmpty(tok))
        {
            W("FATAL no session token in this editor process");
            EditorApplication.Exit(2);
            yield break;
        }

        // 1) 已购列表里找 344832
        using (var req = Req(Api + "/purchases?offset=0&limit=50&query=Factory", "GET", null))
        {
            yield return req.SendWebRequest();
            W("purchases status=" + req.responseCode + " len="
                + (req.downloadHandler.text ?? "").Length);
            if (req.responseCode == 200)
                W("purchases.body=" + Truncate(req.downloadHandler.text, 600));
        }

        // 2) 产品信息（确认 token 能读到该 product）
        using (var req = Req(Api + "/product/" + ProductId, "GET", null))
        {
            yield return req.SendWebRequest();
            W("product status=" + req.responseCode
                + " body=" + Truncate(req.downloadHandler.text, 300));
        }

        // 3) 下载信息（真实端点，来自 AssetStoreRestAPI.k_DownloadInfoUri）
        string downloadUrl = null;
        using (var req = Req(Api + "/legacy-package-download-info/" + ProductId, "GET", null))
        {
            yield return req.SendWebRequest();
            string body = req.downloadHandler.text ?? "";
            W("download-info status=" + req.responseCode + " len=" + body.Length);
            if (req.responseCode == 200)
            {
                W("download-info.body=" + Truncate(Redact(body), 500));
                downloadUrl = ExtractUrl(body);
            }
        }

        if (string.IsNullOrEmpty(downloadUrl))
        {
            W("no download url; stop (see body above for the API's own message)");
            EditorApplication.Exit(3);
            yield break;
        }

        // 4) 下载
        W("downloading to " + PkgPath);
        using (var req = UnityWebRequest.Get(downloadUrl))
        {
            req.downloadHandler = new DownloadHandlerFile(PkgPath);
            var op = req.SendWebRequest();
            double last = 0;
            while (!op.isDone)
            {
                if (EditorApplication.timeSinceStartup - last > 15)
                {
                    last = EditorApplication.timeSinceStartup;
                    W("download progress=" + (req.downloadProgress * 100).ToString("F1") + "%");
                }
                yield return null;
            }
            W("download status=" + req.responseCode + " bytes="
                + (File.Exists(PkgPath) ? new FileInfo(PkgPath).Length : 0));
            if (req.responseCode != 200) { EditorApplication.Exit(4); yield break; }
        }

        // 5) 导入
        W("importing package (this takes a while for 2.8GB)");
        AssetDatabase.ImportPackage(PkgPath, false);
        yield return null;
        W("import triggered; editor will keep running to finish import");
    }

    static string Truncate(string s, int n)
        => string.IsNullOrEmpty(s) ? "" : (s.Length <= n ? s : s.Substring(0, n) + "…");

    /// <summary>下载 URL 自带签名参数，落日志前抹掉 query。</summary>
    static string Redact(string body)
    {
        int i = body.IndexOf("?", StringComparison.Ordinal);
        return i < 0 ? body : body.Substring(0, i) + "?<redacted>";
    }

    static string ExtractUrl(string json)
    {
        const string key = "\"url\"";
        int i = json.IndexOf(key, StringComparison.Ordinal);
        if (i < 0) return null;
        int q1 = json.IndexOf('"', i + key.Length + 1);
        if (q1 < 0) return null;
        int q2 = json.IndexOf('"', q1 + 1);
        return q2 < 0 ? null : json.Substring(q1 + 1, q2 - q1 - 1);
    }
}

/// <summary>最小 editor coroutine 驱动（EditorApplication.update 泵浦）。</summary>
public static class EditorCoroutine
{
    public static void Start(IEnumerator routine)
    {
        void Tick()
        {
            try
            {
                // yield 的若是尚未完成的异步操作，本帧不推进（等它 isDone）
                if (routine.Current is AsyncOperation op && !op.isDone) return;
                if (!routine.MoveNext())
                    EditorApplication.update -= Tick;
            }
            catch (Exception e)
            {
                EditorApplication.update -= Tick;
                Debug.LogError("[acquire] coroutine error: " + e);
                File.AppendAllText("/tmp/diogenes-store-acquire.txt",
                    "coroutine error: " + e.Message + "\n");
                EditorApplication.Exit(5);
            }
        }
        EditorApplication.update += Tick;
    }
}
