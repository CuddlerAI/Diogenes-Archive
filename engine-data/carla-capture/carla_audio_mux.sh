#!/bin/bash
# 给 CARLA 出的 mp4 配环境声轨。
#
# CARLA 不输出音频，而 docs/69 §8.1 要求「有声音、声音连续、无杂音」。
# **不能用静音轨糊弄**——Unity 线刚踩过这个坑（QC 的音轨规则只查有没有轨、
# 不查是不是静音，那条规则本身正在修）。这里合成的是有实际能量的城市环境底噪：
# 棕噪 + 低通（模拟远处交通与风），电平压到 -30dBFS 左右，不掩盖内容也不为零。
#
# 声轨来源如实写进 provenance：合成环境声，非场景实录。
set -e
CLIP_DIR="$1"
DUR="${2:-60}"
TMP=$(mktemp -d)
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "anoisesrc=d=$DUR:c=brown:r=48000:a=0.06" \
  -af "lowpass=f=900,highpass=f=45,volume=0.5" \
  -c:a aac -b:a 128k "$TMP/amb.m4a"
for n in 1 2 3 4; do
  V="$CLIP_DIR/video_$n.mp4"
  [ -f "$V" ] || continue
  ffmpeg -hide_banner -loglevel error -y -i "$V" -i "$TMP/amb.m4a" \
    -map 0:v:0 -map 1:a:0 -c:v copy -c:a aac -b:a 128k -shortest "$V.tmp.mp4"
  mv "$V.tmp.mp4" "$V"
done
rm -rf "$TMP"
echo "audio muxed: $CLIP_DIR"
