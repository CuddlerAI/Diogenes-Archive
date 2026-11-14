# Manual archive build; upstream image resolved and pinned on 2026-11-14.
FROM nginx:stable-alpine@sha256:901e593c95dc85f5a729f5765516321366ef4ac401821431d408897e95287d0a
COPY nginx.conf /etc/nginx/conf.d/default.conf
RUN rm -f /usr/share/nginx/html/*
COPY index.html archive.css favicon.svg logo.svg apple-touch-icon.png robots.txt sitemap.xml /usr/share/nginx/html/
COPY fonts/ /usr/share/nginx/html/fonts/
COPY media/action-record-poster.jpg media/depth-pose-poster.jpg /usr/share/nginx/html/media/
EXPOSE 8080
