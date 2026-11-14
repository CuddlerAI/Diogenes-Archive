# Diogenes Lab project archive

项目关闭后的静态展示站。首页保留两段既有录制演示，将访问者导向
[GitHub](https://github.com/CuddlerAI/Diogenes-Archive) 与
[Hugging Face](https://huggingface.co/DiogenesLab)。数据是否公开、下载条件与许可由各仓库说明，
页面不把尚未完成的迁移描述为已完成。

## 页面与资源

- `index.html`、`archive.css`：中英文同页的归档入口，无 JavaScript、业务 API、分析脚本或表单。
- `media/`：首页使用既有本地海报，视频直连已公开的 Hugging Face A1/A3 演示；`preload="none"`，用户点击后才下载视频。MP4 不包含在此源码归档或镜像中。
- `fonts/`：既有自托管字体及许可。
- `_redirects`：本地预览与旧静态托管的历史路由跳转。
- `nginx.conf`：容器中的同等历史路由跳转、静态缓存、安全响应头及 `/health`。
- `Dockerfile`、`.dockerignore`：显式打包首页和必要资源；旧产品页、运维配置及 README 不进入 Web 根目录。

旧产品页面与退役 Cloudflare 托管配置不包含在此归档中。GitHub 自动部署任务保持关闭。

## 本地预览

在仓库根目录运行已有路由预览工具：

```bash
python3 tools/preview.py website 8788
```

浏览 `http://localhost:8788`。该工具用于页面和跳转检查，不模拟 Nginx 安全响应头、缓存或视频 Range 请求。

如需验证最终服务容器：

```bash
docker build -t diogenes-archive:local website
docker run --rm -p 8080:8080 diogenes-archive:local
```

## 新加坡 Cloud Run 交付

目标为 `asia-southeast1` 的单个请求计费服务，最小实例数 0、最大实例数 1。
Cloud Run 没有名为 `micro` 的机器类型；CPU/内存与执行环境应按当前官方约束设置。
保持静态站、无后台任务、无数据库、无 VPC connector、无负载均衡器。

运行时监听 `8080`，部署时必须保持该端口。发布时把 Dockerfile 的上游镜像标签解析并固定为审阅过的 digest。
构建和发布为手动运维，不依赖 Cloud Build 或 GitHub Actions。

Cloud Run 部署时会导入容器镜像。确认 revision 部署成功后，可删除源 Artifact Registry 镜像，
已有 revision 继续使用已导入副本；未来重部署则需要从保留源码重新构建并临时上传镜像。
参见 [Cloud Run container deployment](https://cloud.google.com/run/docs/deploying)。
服务缩到零可消除闲置实例费用，但静态资源流量、构建、镜像存储等仍应单独核算，不能承诺绝对零费用。

网站运行状态与数据发布范围以实际站点和 Hugging Face 数据卡为准。
